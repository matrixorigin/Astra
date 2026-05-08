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
  cancelRun,
  getSessionState,
  getSessionTranscript,
  type TranscriptItem,
} from '@/lib/api/session-client';
import {
  applyRunEventsTransaction,
  applyTranscriptItemsTransaction,
  clearDeviceLocalState,
  readWatermark,
  SSE_CLIENT_DEAD_TIMEOUT_MS,
  subscribeWatermarks,
} from '@/lib/session-cache/indexeddb';
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

function transcriptItemsToMessages(items: TranscriptItem[]): ChatMessage[] {
  return items.map((item) => ({
    id: `transcript_${item.item_seq}`,
    role: item.role as ChatMessage['role'],
    content: item.content,
    timestamp: Date.parse(item.created_at) || Date.now(),
  }));
}

export type ConnectionState = 'idle' | 'streaming' | 'error';

type UseChatStreamReturn = WorkspaceState & {
  sendMessage: (content: string) => void;
  stop: () => void;
  reset: () => void;
  connectionState: ConnectionState;
  dismissError: () => void;
  contextSummary: {
    usedTokens: number;
    budgetTokens: number;
    droppedCount: number;
    zones: Array<{ zone: string; usedTokens: number; budgetTokens: number }>;
  };
  askUserPrompt: {
    requestId: string;
    question: string;
    choices: string[];
  } | null;
  answerAskUser: (answer: string) => void;
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
  const [contextSummary, setContextSummary] = useState<UseChatStreamReturn['contextSummary']>({
    usedTokens: 0,
    budgetTokens: 7300,
    droppedCount: 0,
    zones: [],
  });
  const [askUserPrompt, setAskUserPrompt] = useState<UseChatStreamReturn['askUserPrompt']>(null);

  // Refs for mutable state during stream processing
  const sseClientRef = useRef<SSEClient | null>(null);
  const assistantIdRef = useRef<string>('');
  const accumulatedTextRef = useRef('');
  const accumulatedThinkingRef = useRef('');
  const toolCallMapRef = useRef<Map<string, ToolCall>>(new Map());
  const lastUserMessageRef = useRef('');
  const sawErrorEventRef = useRef(false);
  const sessionIdRef = useRef<string | null>(config.sessionId ?? null);
  const runIdRef = useRef<string | null>(null);
  const runEventLastOkIdxRef = useRef(-1);
  // Track config.sessionId to detect external changes
  const configSessionIdRef = useRef(config.sessionId);

  useEffect(() => {
    sessionIdRef.current = sessionId;
  }, [sessionId]);

  useEffect(() => {
    runIdRef.current = runId;
  }, [runId]);

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
      setContextSummary({ usedTokens: 0, budgetTokens: 7300, droppedCount: 0, zones: [] });
      setAskUserPrompt(null);
      accumulatedTextRef.current = '';
      accumulatedThinkingRef.current = '';
      lastUserMessageRef.current = '';
      sawErrorEventRef.current = false;
      runEventLastOkIdxRef.current = -1;
      toolCallMapRef.current.clear();
    }
  }, [config.sessionId]);

  useEffect(() => {
    return subscribeWatermarks((message) => {
      if (message.sessionId === sessionIdRef.current) {
        runEventLastOkIdxRef.current = Math.max(
          runEventLastOkIdxRef.current,
          message.runEventHighWatermark,
        );
      }
    });
  }, []);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      sseClientRef.current?.close();
      sseClientRef.current = null;
    };
  }, []);

  const processEvent = useCallback(
    (event: StreamEvent) => {
      const eventType = (event as { type: string }).type;
      if (eventType === 'ping') {
        return;
      }
      if (eventType === 'device_revoked' || eventType === 'device_lease_expired') {
        void clearDeviceLocalState();
        setError(`${eventType}: device session ended`);
        return;
      }
      if (eventType === 'context_manifest') {
        const manifest = event as StreamEvent & {
          total_estimated_tokens?: number;
          budget_tokens?: number;
          dropped_count?: number;
          zones?: Array<{ zone: string; used_tokens?: number; budget_tokens?: number }>;
        };
        setContextSummary({
          usedTokens: manifest.total_estimated_tokens ?? 0,
          budgetTokens: manifest.budget_tokens ?? 7300,
          droppedCount: manifest.dropped_count ?? 0,
          zones:
            manifest.zones?.map((zone) => ({
              zone: zone.zone,
              usedTokens: zone.used_tokens ?? 0,
              budgetTokens: zone.budget_tokens ?? 0,
            })) ?? [],
        });
        return;
      }
      if (eventType === 'user_prompt_request') {
        const prompt = event as StreamEvent & {
          request_id?: string;
          question?: string;
          choices?: Array<{ label?: string; value?: string } | string>;
        };
        setAskUserPrompt({
          requestId: prompt.request_id ?? `prompt_${Date.now()}`,
          question: prompt.question ?? 'Choose the next action',
          choices:
            prompt.choices?.map((choice) =>
              typeof choice === 'string' ? choice : (choice.value ?? choice.label ?? ''),
            ).filter(Boolean).slice(0, 3) ?? [],
        });
        return;
      }

      switch (event.type) {
        case 'session_info': {
          setSessionId(event.session_id);
          setRunId(event.run_id ?? null);
          sessionIdRef.current = event.session_id;
          runIdRef.current = event.run_id ?? null;
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
          const subtasks: PlanSubtask[] = event.plan.subtasks.map((s) => {
            const raw = s as PlanSubtask & Record<string, unknown>;
            const subtask = {
              id: s.id,
              title: s.title,
              status: (s.status as PlanSubtask['status']) ?? 'pending',
            } as PlanSubtask & Record<string, unknown>;
            for (const key of [
              'parent_id',
              'parentId',
              'parent_todo_id',
              'parentTodoId',
              'section',
              'depth',
              'summary',
            ]) {
              if (raw[key] !== undefined) {
                subtask[key] = raw[key];
              }
            }
            return subtask;
          });
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

      const sid = event.type === 'session_info' ? event.session_id : sessionIdRef.current;
      const rid = event.type === 'session_info' ? event.run_id : runIdRef.current;
      if (sid && rid && typeof event.index === 'number') {
        void applyRunEventsTransaction(sid, rid, [event], runEventLastOkIdxRef.current).then(
          (result) => {
            runEventLastOkIdxRef.current = result.lastOkIdx;
            if (result.gapDetected) {
              setError(`Event gap detected; reconnect from ${result.reconnectLastIndex}`);
              sseClientRef.current?.close();
            }
          },
        );
      }
    },
    [],
  );

  useEffect(() => {
    const sessionForHydration = config.sessionId;
    if (!sessionForHydration) return;
    const hydratedSessionId = sessionForHydration;
    let cancelled = false;

    async function hydrateColdStart() {
      const watermark = await readWatermark(hydratedSessionId);
      const deviceId = getOrCreateDeviceId();
      const deviceFingerprint = getDeviceFingerprint();
      const state = await getSessionState(hydratedSessionId, {
        knownStateRevision: watermark?.stateRevision ?? 0,
        clientCacheEmpty: !watermark,
        deviceId,
        deviceFingerprint,
      });
      if (cancelled) return;
      if (state.transcript_high_watermark > 0) {
        const transcript = await getSessionTranscript(hydratedSessionId, undefined, 100);
        if (cancelled) return;
        await applyTranscriptItemsTransaction(hydratedSessionId, transcript.items);
        setMessages(transcriptItemsToMessages(transcript.items));
      }
      if (state.run_event_replay_required && state.active_run) {
        const activeRunId = state.active_run.run_id;
        setRunId(activeRunId);
        runIdRef.current = activeRunId;
        runEventLastOkIdxRef.current = state.active_run.replay_start_event_idx - 1;
        const replayClient = new SSEClient({
          url: `/api/backend/chat/runs/${activeRunId}/stream?last_index=0`,
          onEvent: processEvent,
          onStateChange: (nextState) => {
            if (nextState === 'error') {
              setConnectionState('error');
            } else if (nextState === 'connected') {
              setConnectionState('streaming');
            } else if (nextState === 'disconnected' && !sawErrorEventRef.current) {
              setConnectionState('idle');
            }
          },
          maxRetries: 0,
          heartbeatTimeoutMs: SSE_CLIENT_DEAD_TIMEOUT_MS,
        } as ConstructorParameters<typeof SSEClient>[0] & { heartbeatTimeoutMs: number });
        sseClientRef.current?.close();
        sseClientRef.current = replayClient;
        void replayClient.connect();
      } else if (watermark) {
        runEventLastOkIdxRef.current = watermark.runEventHighWatermark;
      }
    }

    hydrateColdStart().catch((err) => {
      setError(err instanceof Error ? err.message : 'Failed to hydrate session history');
      setConnectionState('error');
    });
    return () => {
      cancelled = true;
    };
  }, [config.sessionId, processEvent]);

  const stop = useCallback(() => {
    const activeRunId = runIdRef.current;
    if (activeRunId) {
      void cancelRun(activeRunId).catch((err) => {
        setError(err instanceof Error ? err.message : 'Failed to cancel run');
      });
    }
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
        heartbeatTimeoutMs: SSE_CLIENT_DEAD_TIMEOUT_MS,
      } as ConstructorParameters<typeof SSEClient>[0] & { heartbeatTimeoutMs: number });

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
    setContextSummary({ usedTokens: 0, budgetTokens: 7300, droppedCount: 0, zones: [] });
    accumulatedTextRef.current = '';
    accumulatedThinkingRef.current = '';
    lastUserMessageRef.current = '';
    toolCallMapRef.current.clear();
    runEventLastOkIdxRef.current = -1;
  }, [config.sessionId]);

  const dismissError = useCallback(() => {
    setError(null);
  }, []);

  const answerAskUser = useCallback(
    (answer: string) => {
      setAskUserPrompt(null);
      sendMessage(answer);
    },
    [sendMessage],
  );

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
    contextSummary,
    askUserPrompt,
    answerAskUser,
    sendMessage,
    stop,
    reset,
    dismissError,
  };
}

function getOrCreateDeviceId(): string {
  const key = 'astra_device_id';
  const existing = localStorage.getItem(key);
  if (existing) return existing;
  const id =
    typeof crypto !== 'undefined' && 'randomUUID' in crypto
      ? crypto.randomUUID()
      : `device_${Date.now()}_${Math.random().toString(16).slice(2)}`;
  localStorage.setItem(key, id);
  return id;
}

function getDeviceFingerprint(): string {
  const raw = [
    navigator.userAgent,
    navigator.language,
    `${screen.width}x${screen.height}`,
    Intl.DateTimeFormat().resolvedOptions().timeZone,
  ].join('|');
  let hash = 0;
  for (let i = 0; i < raw.length; i += 1) {
    hash = (hash * 31 + raw.charCodeAt(i)) >>> 0;
  }
  return `web-${hash.toString(16)}`;
}
