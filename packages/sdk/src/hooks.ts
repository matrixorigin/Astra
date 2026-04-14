import { useState, useRef, useCallback, useEffect } from 'react';
import type {
  StreamEvent,
  ChatMessage,
  ToolCall,
  PlanState,
  TokenUsage,
  ChatConfig,
  ConnectionState,
} from './types';
import { AstraClient } from './client';

// ─── useAstraChat ──────────────────────────────────────────────────

export type UseAstraChatConfig = ChatConfig & {
  client: AstraClient;
};

export type UseAstraChatReturn = {
  sessionId: string | null;
  runId: string | null;
  messages: ChatMessage[];
  toolCalls: ToolCall[];
  isStreaming: boolean;
  error: string | null;
  plan: PlanState | null;
  usage: TokenUsage;
  agentEvents: StreamEvent[];
  connectionState: ConnectionState | 'idle';
  sendMessage: (content: string) => void;
  stop: () => void;
  reset: () => void;
};

const emptyUsage: TokenUsage = {
  promptTokens: 0,
  completionTokens: 0,
  totalTokens: 0,
  cacheCreationTokens: 0,
  cacheReadTokens: 0,
};

/**
 * React hook for streaming Astra chat interactions.
 *
 * Manages messages, tool calls, plan state, usage tracking, and agent events.
 * Connects via SSE to the Astra server for real-time streaming.
 */
export function useAstraChat(config: UseAstraChatConfig): UseAstraChatReturn {
  const [sessionId, setSessionId] = useState<string | null>(config.sessionId ?? null);
  const [runId, setRunId] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [toolCalls, setToolCalls] = useState<ToolCall[]>([]);
  const [isStreaming, setIsStreaming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<PlanState | null>(null);
  const [usage, setUsage] = useState<TokenUsage>(emptyUsage);
  const [agentEvents, setAgentEvents] = useState<StreamEvent[]>([]);
  const [connectionState, setConnectionState] = useState<ConnectionState | 'idle'>('idle');

  const controllerRef = useRef<AbortController | null>(null);
  const accumulatedTextRef = useRef('');
  const accumulatedThinkingRef = useRef('');
  const toolCallMapRef = useRef(new Map<string, ToolCall>());
  const assistantIdRef = useRef(0);

  // Reset on session change
  useEffect(() => {
    if (config.sessionId !== sessionId) {
      reset();
      setSessionId(config.sessionId ?? null);
    }
  }, [config.sessionId]);

  const processEvent = useCallback((event: StreamEvent) => {
    switch (event.type) {
      case 'session_info':
        setSessionId(event.session_id);
        if (event.run_id) setRunId(event.run_id);
        break;

      case 'run_started':
        if (event.run_id) setRunId(event.run_id);
        break;

      case 'text_delta':
        accumulatedTextRef.current += event.content;
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === 'assistant' && last.streaming) {
            return [
              ...prev.slice(0, -1),
              { ...last, content: accumulatedTextRef.current },
            ];
          }
          return prev;
        });
        break;

      case 'reasoning_delta':
        accumulatedThinkingRef.current += event.content;
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === 'assistant' && last.streaming) {
            return [
              ...prev.slice(0, -1),
              {
                ...last,
                thinking: {
                  content: accumulatedThinkingRef.current,
                  done: false,
                },
              },
            ];
          }
          return prev;
        });
        break;

      case 'reasoning_done':
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === 'assistant' && last.thinking) {
            return [
              ...prev.slice(0, -1),
              { ...last, thinking: { ...last.thinking, done: true } },
            ];
          }
          return prev;
        });
        break;

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
          const updated: ToolCall = {
            ...existing,
            result: event.result,
            status: 'done',
            finishedAt: Date.now(),
          };
          toolCallMapRef.current.set(event.call_id, updated);
          setToolCalls(Array.from(toolCallMapRef.current.values()));
        }
        break;
      }

      case 'usage':
        setUsage((prev) => ({
          promptTokens: prev.promptTokens + event.prompt_tokens,
          completionTokens: prev.completionTokens + event.completion_tokens,
          totalTokens:
            prev.totalTokens + event.prompt_tokens + event.completion_tokens,
          cacheCreationTokens:
            prev.cacheCreationTokens + (event.cache_creation_tokens ?? 0),
          cacheReadTokens:
            prev.cacheReadTokens + (event.cache_read_tokens ?? 0),
        }));
        break;

      case 'plan_created':
      case 'plan_revised':
        setPlan({
          planId: event.plan.plan_id,
          title: event.plan.title,
          subtasks: event.plan.subtasks.map((s: { id: string; title: string; status?: string }) => ({
            id: s.id,
            title: s.title,
            status: (s.status ?? 'pending') as 'pending' | 'running' | 'done' | 'error',
          })),
        });
        break;

      case 'plan_step_start':
        setPlan((prev) =>
          prev
            ? {
                ...prev,
                activeStepId: event.subtask_id ?? event.step,
                subtasks: prev.subtasks.map((s) =>
                  s.id === (event.subtask_id ?? event.step) ? { ...s, status: 'running' as const } : s,
                ),
              }
            : null,
        );
        break;

      case 'plan_step_done':
        setPlan((prev) =>
          prev
            ? {
                ...prev,
                subtasks: prev.subtasks.map((s) =>
                  s.id === (event.subtask_id ?? event.step)
                    ? { ...s, status: (event.result === 'error' ? 'error' : 'done') as 'done' | 'error' }
                    : s,
                ),
              }
            : null,
        );
        break;

      case 'error':
        setError(event.message);
        break;

      case 'run_finished':
      case 'run_cancelled':
        setIsStreaming(false);
        setConnectionState('idle');
        // Finalize the assistant message
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === 'assistant' && last.streaming) {
            return [...prev.slice(0, -1), { ...last, streaming: false }];
          }
          return prev;
        });
        break;

      case 'agent_delegated':
      case 'agent_spawned':
      case 'agent_progress':
      case 'agent_completed':
        setAgentEvents((prev) => [...prev, event]);
        break;
    }
  }, []);

  const sendMessage = useCallback(
    (content: string) => {
      // Add user message
      const userMsg: ChatMessage = {
        id: `user-${Date.now()}`,
        role: 'user',
        content,
        timestamp: Date.now(),
      };

      // Create placeholder assistant message
      accumulatedTextRef.current = '';
      accumulatedThinkingRef.current = '';
      toolCallMapRef.current.clear();
      const assistantMsg: ChatMessage = {
        id: `assistant-${++assistantIdRef.current}`,
        role: 'assistant',
        content: '',
        timestamp: Date.now(),
        streaming: true,
      };

      setMessages((prev) => [...prev, userMsg, assistantMsg]);
      setToolCalls([]);
      setError(null);
      setIsStreaming(true);

      // Abort previous stream
      controllerRef.current?.abort();
      controllerRef.current = new AbortController();

      const sseClient = config.client.streamChat(
        {
          message: content,
          sessionId: sessionId ?? undefined,
          model: config.model,
        },
        {
          onEvent: processEvent,
          onStateChange: (state) => setConnectionState(state),
          signal: controllerRef.current.signal,
        },
      );

      // Store SSE client for cleanup
      const currentController = controllerRef.current;
      currentController.signal.addEventListener('abort', () => {
        sseClient.close();
      });
    },
    [config.client, config.model, sessionId, processEvent],
  );

  const stop = useCallback(() => {
    controllerRef.current?.abort();
    setIsStreaming(false);
    setConnectionState('idle');
    // Finalize assistant message
    setMessages((prev) => {
      const last = prev[prev.length - 1];
      if (last?.role === 'assistant' && last.streaming) {
        return [...prev.slice(0, -1), { ...last, streaming: false }];
      }
      return prev;
    });
  }, []);

  const reset = useCallback(() => {
    controllerRef.current?.abort();
    setSessionId(null);
    setRunId(null);
    setMessages([]);
    setToolCalls([]);
    setIsStreaming(false);
    setError(null);
    setPlan(null);
    setUsage(emptyUsage);
    setAgentEvents([]);
    setConnectionState('idle');
    accumulatedTextRef.current = '';
    accumulatedThinkingRef.current = '';
    toolCallMapRef.current.clear();
  }, []);

  return {
    sessionId,
    runId,
    messages,
    toolCalls,
    isStreaming,
    error,
    plan,
    usage,
    agentEvents,
    connectionState,
    sendMessage,
    stop,
    reset,
  };
}

// ─── useAstraRun ───────────────────────────────────────────────────

export type UseAstraRunConfig = {
  client: AstraClient;
  runId: string;
  pollIntervalMs?: number;
};

export type UseAstraRunReturn = {
  status: string | null;
  events: StreamEvent[];
  isPolling: boolean;
  error: string | null;
  refresh: () => void;
};

/**
 * React hook for polling run status and events.
 */
export function useAstraRun(config: UseAstraRunConfig): UseAstraRunReturn {
  const [status, setStatus] = useState<string | null>(null);
  const [events, setEvents] = useState<StreamEvent[]>([]);
  const [isPolling, setIsPolling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const eventIndexRef = useRef(0);

  const refresh = useCallback(async () => {
    try {
      setIsPolling(true);
      const [runStatus, newEvents] = await Promise.all([
        config.client.getRunStatus(config.runId),
        config.client.getRunEvents(config.runId, eventIndexRef.current),
      ]);
      setStatus(runStatus.status);
      if (newEvents.length > 0) {
        eventIndexRef.current += newEvents.length;
        setEvents((prev) => [...prev, ...newEvents]);
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to poll run');
    } finally {
      setIsPolling(false);
    }
  }, [config.client, config.runId]);

  useEffect(() => {
    const interval = setInterval(refresh, config.pollIntervalMs ?? 2000);
    refresh();
    return () => clearInterval(interval);
  }, [refresh, config.pollIntervalMs]);

  return { status, events, isPolling, error, refresh };
}

// ─── useAstraWebSocket ─────────────────────────────────────────────

export { AstraWebSocket } from './websocket';
export type { AstraWebSocketOptions, ToolApproval } from './websocket';
