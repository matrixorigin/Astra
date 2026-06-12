import { useState, useRef, useCallback, useEffect } from "react";
import type {
  StreamEvent,
  ChatMessage,
  ToolCall,
  PlanState,
  TokenUsage,
  ChatConfig,
  ConnectionState,
  WorkspaceBinding,
  ExecutorBinding,
} from "./types";
import { AstraClient } from "./client";
import {
  EXECUTION_BOUNDARY_WAIT_REASONS,
  isExecutionBoundaryWait,
  extractWaitingReason,
  extractBlockedReason,
} from "./lifecycle-utils";

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
  runStatus: string | null;
  waitingFor: string | null;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport: string | null;
  fallbackPolicy: string | null;
  connectionState: ConnectionState | "idle";
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

type RunExecutionBoundary = {
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string | null;
  fallbackPolicy?: string | null;
};

function runBoundaryFieldsFromEvent(event: StreamEvent): RunExecutionBoundary {
  const source = event as StreamEvent & {
    workspace?: WorkspaceBinding;
    executor?: ExecutorBinding;
    transport?: string | null;
    fallback_policy?: string | null;
  };
  const fields: RunExecutionBoundary = {};
  if (source.workspace !== undefined) fields.workspace = source.workspace;
  if (source.executor !== undefined) fields.executor = source.executor;
  if (source.transport !== undefined) fields.transport = source.transport;
  if (source.fallback_policy !== undefined) {
    fields.fallbackPolicy = source.fallback_policy;
  }
  return fields;
}

function runIdFromEvent(event: StreamEvent): string | undefined {
  const source = event as StreamEvent & { run_id?: string };
  return source.run_id?.trim() || undefined;
}

function valueToToolString(value: unknown): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function executionFieldsFromEvent(event: StreamEvent): Partial<ToolCall> {
  const source = event as StreamEvent & {
    workspace?: ToolCall["workspace"];
    executor?: ToolCall["executor"];
    transport?: string;
    fallback_policy?: string;
    route?: string;
    error_kind?: string;
    blocked?: boolean;
    duration_ms?: number;
  };
  const fields: Partial<ToolCall> = {};
  if (source.workspace !== undefined) fields.workspace = source.workspace;
  if (source.executor !== undefined) fields.executor = source.executor;
  if (source.transport !== undefined) fields.transport = source.transport;
  if (source.fallback_policy !== undefined) {
    fields.fallbackPolicy = source.fallback_policy;
  }
  if (source.route !== undefined) fields.route = source.route;
  if (source.error_kind !== undefined) fields.errorKind = source.error_kind;
  if (source.blocked !== undefined) fields.blocked = source.blocked;
  if (source.duration_ms !== undefined) fields.durationMs = source.duration_ms;
  return fields;
}

function compactToolCall(call: ToolCall): ToolCall {
  return Object.fromEntries(
    Object.entries(call).filter(([, value]) => value !== undefined),
  ) as ToolCall;
}

/**
 * React hook for streaming Astra chat interactions.
 *
 * Manages messages, tool calls, plan state, usage tracking, and agent events.
 * Connects via SSE to the Astra server for real-time streaming.
 */
export function useAstraChat(config: UseAstraChatConfig): UseAstraChatReturn {
  const [sessionId, setSessionId] = useState<string | null>(
    config.sessionId ?? null,
  );
  const [runId, setRunId] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [toolCalls, setToolCalls] = useState<ToolCall[]>([]);
  const [isStreaming, setIsStreaming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<PlanState | null>(null);
  const [usage, setUsage] = useState<TokenUsage>(emptyUsage);
  const [agentEvents, setAgentEvents] = useState<StreamEvent[]>([]);
  const [runStatus, setRunStatus] = useState<string | null>(null);
  const [waitingFor, setWaitingFor] = useState<string | null>(null);
  const [workspace, setWorkspace] = useState<WorkspaceBinding | undefined>();
  const [executor, setExecutor] = useState<ExecutorBinding | undefined>();
  const [transport, setTransport] = useState<string | null>(null);
  const [fallbackPolicy, setFallbackPolicy] = useState<string | null>(null);
  const [connectionState, setConnectionState] = useState<
    ConnectionState | "idle"
  >("idle");

  const controllerRef = useRef<AbortController | null>(null);
  const accumulatedTextRef = useRef("");
  const accumulatedThinkingRef = useRef("");
  const toolCallMapRef = useRef(new Map<string, ToolCall>());
  const assistantIdRef = useRef(0);
  const streamGenerationRef = useRef(0);

  // Reset on session change
  useEffect(() => {
    if (config.sessionId !== sessionId) {
      reset();
      setSessionId(config.sessionId ?? null);
    }
  }, [config.sessionId]);

  const processEvent = useCallback((event: StreamEvent) => {
    // Guard against stale events from an aborted stream leaking into the
    // new stream's state (accumulatedTextRef, toolCallMapRef, etc.).
    const generation = streamGenerationRef.current;
    const isStale = () => streamGenerationRef.current !== generation;

    const upsertToolCall = (
      callId: string,
      build: (existing: ToolCall | undefined) => ToolCall,
    ) => {
      const existing = toolCallMapRef.current.get(callId);
      toolCallMapRef.current.set(callId, compactToolCall(build(existing)));
      setToolCalls(Array.from(toolCallMapRef.current.values()));
    };

    const applyRunExecutionBoundary = () => {
      const fields = runBoundaryFieldsFromEvent(event);
      if (fields.workspace !== undefined) setWorkspace(fields.workspace);
      if (fields.executor !== undefined) setExecutor(fields.executor);
      if (fields.transport !== undefined) setTransport(fields.transport);
      if (fields.fallbackPolicy !== undefined) {
        setFallbackPolicy(fields.fallbackPolicy);
      }
    };

    // Drop events from an earlier stream that completed after abort.
    // Without this guard, a stale text_delta from the aborted stream can
    // corrupt accumulatedTextRef of the new stream.
    // Placed BEFORE the run_blocked_* handler so stale run-blocked events
    // cannot leak execution bindings into the new stream's state.
    if (isStale()) {
      return;
    }

    if (event.type.startsWith("run_blocked_")) {
      applyRunExecutionBoundary();
      const reason = extractBlockedReason(event) ?? "blocked";
      const blockedRunId = runIdFromEvent(event);
      setRunStatus("blocked");
      setWaitingFor(reason);
      if (blockedRunId) setRunId(blockedRunId);
      return;
    }

    switch (event.type) {
      case "session_info":
        setSessionId(event.session_id);
        if (event.run_id) setRunId(event.run_id);
        break;

      case "run_started":
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        setRunStatus("running");
        setWaitingFor(null);
        break;

      case "workspace_bound":
      case "executor_bound":
      case "executor_status_changed":
        applyRunExecutionBoundary();
        break;

      case "run_paused":
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        setRunStatus("paused");
        break;

      case "run_resumed":
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        setRunStatus("running");
        setWaitingFor(null);
        break;

      case "run_waiting": {
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        const reason = extractWaitingReason(event);
        setRunStatus("waiting");
        setWaitingFor(reason);
        break;
      }

      case "run_error":
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        setRunStatus("failed");
        setWaitingFor(null);
        setError(event.message ?? event.error ?? "Astra run failed.");
        setIsStreaming(false);
        setConnectionState("idle");
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return [...prev.slice(0, -1), { ...last, streaming: false }];
          }
          return prev;
        });
        break;

      case "run_interrupted":
        applyRunExecutionBoundary();
        if (event.run_id) setRunId(event.run_id);
        setRunStatus("paused");
        setWaitingFor(event.waiting_for ?? "user_resume");
        setIsStreaming(false);
        setConnectionState("idle");
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return [...prev.slice(0, -1), { ...last, streaming: false }];
          }
          return prev;
        });
        break;

      case "text_delta":
        accumulatedTextRef.current += event.content;
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return [
              ...prev.slice(0, -1),
              { ...last, content: accumulatedTextRef.current },
            ];
          }
          return prev;
        });
        break;

      case "reasoning_delta":
        accumulatedThinkingRef.current += event.content;
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
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

      case "reasoning_done":
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.thinking) {
            return [
              ...prev.slice(0, -1),
              { ...last, thinking: { ...last.thinking, done: true } },
            ];
          }
          return prev;
        });
        break;

      case "tool_call": {
        const toolCall = event.tool_call;
        const fn = toolCall.function;
        const callId = toolCall.id ?? toolCall.call_id ?? fn?.id ?? fn?.call_id;
        if (!callId) break;
        upsertToolCall(callId, (existing) => ({
          callId,
          tool:
            fn?.name ??
            toolCall.name ??
            toolCall.tool ??
            existing?.tool ??
            "tool",
          arguments:
            valueToToolString(
              fn?.arguments ?? toolCall.arguments ?? toolCall.args,
            ) ?? existing?.arguments,
          status: existing?.status ?? "running",
          startedAt: existing?.startedAt ?? Date.now(),
          ...executionFieldsFromEvent(event),
        }));
        break;
      }

      case "tool_call_start":
      case "tool_transport_started": {
        const args =
          event.type === "tool_call_start"
            ? event.arguments
            : valueToToolString(event.arguments);
        upsertToolCall(event.call_id, (existing) => ({
          ...existing,
          callId: event.call_id,
          tool: event.tool ?? existing?.tool ?? "tool",
          arguments: args ?? existing?.arguments,
          status: "running",
          startedAt: existing?.startedAt ?? Date.now(),
          ...executionFieldsFromEvent(event),
        }));
        break;
      }

      case "tool_routing_decision": {
        upsertToolCall(event.call_id, (existing) => ({
          ...existing,
          callId: event.call_id,
          tool: event.tool ?? existing?.tool ?? "tool",
          status: existing?.status ?? "running",
          startedAt: existing?.startedAt ?? Date.now(),
          ...executionFieldsFromEvent(event),
        }));
        break;
      }

      case "tool_call_end": {
        upsertToolCall(event.call_id, (existing) => ({
          ...existing,
          callId: event.call_id,
          tool: existing?.tool ?? "tool",
          result: event.result,
          status:
            event.success === false || event.error_kind ? "error" : "done",
          startedAt: existing?.startedAt ?? Date.now(),
          finishedAt: Date.now(),
          ...executionFieldsFromEvent(event),
        }));
        break;
      }

      case "tool_transport_completed":
      case "tool_transport_failed": {
        upsertToolCall(event.call_id, (existing) => ({
          ...existing,
          callId: event.call_id,
          tool: event.tool ?? existing?.tool ?? "tool",
          result:
            event.type === "tool_transport_completed"
              ? (valueToToolString(event.result) ?? existing?.result)
              : (event.error ?? existing?.result),
          status:
            event.type === "tool_transport_failed" || event.success === false
              ? "error"
              : "done",
          startedAt: existing?.startedAt ?? Date.now(),
          finishedAt: Date.now(),
          ...executionFieldsFromEvent(event),
        }));
        break;
      }

      case "usage":
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

      case "plan_created":
      case "plan_revised":
        setPlan({
          planId: event.plan.plan_id,
          title: event.plan.title,
          subtasks: event.plan.subtasks.map(
            (s: { id: string; title: string; status?: string }) => ({
              id: s.id,
              title: s.title,
              status: (s.status ?? "pending") as
                | "pending"
                | "running"
                | "done"
                | "error",
            }),
          ),
        });
        break;

      case "plan_step_start":
        setPlan((prev) =>
          prev
            ? {
                ...prev,
                activeStepId: event.subtask_id ?? event.step,
                subtasks: prev.subtasks.map((s) =>
                  s.id === (event.subtask_id ?? event.step)
                    ? { ...s, status: "running" as const }
                    : s,
                ),
              }
            : null,
        );
        break;

      case "plan_step_done":
        setPlan((prev) =>
          prev
            ? {
                ...prev,
                subtasks: prev.subtasks.map((s) =>
                  s.id === (event.subtask_id ?? event.step)
                    ? {
                        ...s,
                        status: (event.result === "error"
                          ? "error"
                          : "done") as "done" | "error",
                      }
                    : s,
                ),
              }
            : null,
        );
        break;

      case "error":
        setError(event.message);
        break;

      case "turn_complete":
        setIsStreaming(false);
        setConnectionState("idle");
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return [...prev.slice(0, -1), { ...last, streaming: false }];
          }
          return prev;
        });
        break;

      case "run_finished":
      case "run_cancelled":
        applyRunExecutionBoundary();
        setIsStreaming(false);
        setConnectionState("idle");
        if (event.run_id) setRunId(event.run_id);
        setRunStatus(
          event.type === "run_cancelled"
            ? "cancelled"
            : (event.status ?? "completed"),
        );
        setWaitingFor(null);
        // Finalize the assistant message
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last?.role === "assistant" && last.streaming) {
            return [...prev.slice(0, -1), { ...last, streaming: false }];
          }
          return prev;
        });
        break;

      case "agent_delegated":
      case "agent_spawned":
      case "agent_waiting":
      case "agent_progress":
      case "agent_completed":
      case "agent_failed":
      case "agent_cancelled":
      case "agent_interrupted":
        setAgentEvents((prev) => [...prev, event]);
        break;
    }
  }, []);

  const sendMessage = useCallback(
    (content: string) => {
      // Bump generation so processEvent from the previous stream is ignored.
      const generation = ++streamGenerationRef.current;

      // Add user message
      const userMsg: ChatMessage = {
        id: `user-${Date.now()}`,
        role: "user",
        content,
        timestamp: Date.now(),
      };

      // Create placeholder assistant message
      accumulatedTextRef.current = "";
      accumulatedThinkingRef.current = "";
      toolCallMapRef.current.clear();
      const assistantMsg: ChatMessage = {
        id: `assistant-${++assistantIdRef.current}`,
        role: "assistant",
        content: "",
        timestamp: Date.now(),
        streaming: true,
      };

      setMessages((prev) => [...prev, userMsg, assistantMsg]);
      setToolCalls([]);
      setError(null);
      setRunStatus("running");
      setWaitingFor(null);
      setIsStreaming(true);

      // Abort previous stream
      controllerRef.current?.abort();
      controllerRef.current = new AbortController();

      const sseClient = config.client.streamChat(
        {
          message: content,
          sessionId: sessionId ?? undefined,
          agentId: config.agentId,
          model: config.model,
          allowSkills: config.allowSkills,
          allowTools: config.allowTools,
          skillSearch: config.skillSearch,
        },
        {
          onEvent: processEvent,
          onStateChange: (state) => setConnectionState(state),
          signal: controllerRef.current.signal,
        },
      );

      // Store SSE client for cleanup
      const currentController = controllerRef.current;
      currentController.signal.addEventListener("abort", () => {
        sseClient.close();
      });
    },
    [
      config.client,
      config.agentId,
      config.model,
      config.allowSkills,
      config.allowTools,
      config.skillSearch,
      sessionId,
      processEvent,
    ],
  );

  const stop = useCallback(() => {
    controllerRef.current?.abort();
    setIsStreaming(false);
    setRunStatus("cancelled");
    setWaitingFor(null);
    setConnectionState("idle");
    // Finalize assistant message
    setMessages((prev) => {
      const last = prev[prev.length - 1];
      if (last?.role === "assistant" && last.streaming) {
        return [...prev.slice(0, -1), { ...last, streaming: false }];
      }
      return prev;
    });
  }, []);

  const reset = useCallback(() => {
    controllerRef.current?.abort();
    setSessionId(null);
    setRunId(null);
    setRunStatus(null);
    setWaitingFor(null);
    setWorkspace(undefined);
    setExecutor(undefined);
    setTransport(null);
    setFallbackPolicy(null);
    setMessages([]);
    setToolCalls([]);
    setIsStreaming(false);
    setError(null);
    setPlan(null);
    setUsage(emptyUsage);
    setAgentEvents([]);
    setConnectionState("idle");
    accumulatedTextRef.current = "";
    accumulatedThinkingRef.current = "";
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
    runStatus,
    waitingFor,
    workspace,
    executor,
    transport,
    fallbackPolicy,
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
      setError(err instanceof Error ? err.message : "Failed to poll run");
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

export { AstraWebSocket } from "./websocket";
export type { AstraWebSocketOptions, ToolApproval } from "./websocket";
