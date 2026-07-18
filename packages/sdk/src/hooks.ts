import { useState, useRef, useCallback, useEffect, useReducer } from "react";
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
  SessionTask,
  AgentActivity,
} from "./types";
import { AstraClient } from "./client";
import {
  extractBlockedReason,
  planStepResultStatus,
  projectRunWaitingState,
  toolTerminalStatus,
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
  tasks: SessionTask[];
  agents: AgentActivity[];
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

function agentActivityFromEvent(event: StreamEvent): AgentActivity {
  const source = event as StreamEvent & {
    agent_id: string;
    run_id?: string;
    parent_run_id?: string;
    agent_type?: string;
    description?: string;
    task?: string;
    status?: string;
    reason?: string;
    error?: string;
    result_summary?: string;
    tool_name?: string;
    turn?: number;
    max_turns?: number;
    total_prompt_tokens?: number;
    total_completion_tokens?: number;
    total_tool_calls?: number;
    total_tokens?: { prompt?: number; completion?: number };
    duration_ms?: number;
    timestamp?: number;
  };
  const status =
    source.status ??
    (event.type === "agent_spawned"
      ? "running"
      : event.type === "agent_delegated"
        ? "delegated"
        : event.type.replace(/^agent_/, ""));
  const activity: AgentActivity = {
    agentId: source.agent_id,
    status,
    updatedAt: source.timestamp ?? Date.now(),
  };
  if (source.run_id) activity.runId = source.run_id;
  if (source.parent_run_id) activity.parentRunId = source.parent_run_id;
  if (source.agent_type) activity.agentType = source.agent_type;
  if (source.description) activity.description = source.description;
  if (source.task) activity.task = source.task;
  if (source.reason) activity.reason = source.reason;
  if (source.error) activity.error = source.error;
  if (source.result_summary) activity.resultSummary = source.result_summary;
  if (source.tool_name) activity.toolName = source.tool_name;
  if (source.turn !== undefined) activity.turn = source.turn;
  if (source.max_turns !== undefined) activity.maxTurns = source.max_turns;
  if (source.total_prompt_tokens !== undefined) {
    activity.totalPromptTokens = source.total_prompt_tokens;
  } else if (source.total_tokens?.prompt !== undefined) {
    activity.totalPromptTokens = source.total_tokens.prompt;
  }
  if (source.total_completion_tokens !== undefined) {
    activity.totalCompletionTokens = source.total_completion_tokens;
  } else if (source.total_tokens?.completion !== undefined) {
    activity.totalCompletionTokens = source.total_tokens.completion;
  }
  if (source.total_tool_calls !== undefined) {
    activity.totalToolCalls = source.total_tool_calls;
  }
  if (source.duration_ms !== undefined) activity.durationMs = source.duration_ms;
  return activity;
}

/**
 * React hook for streaming Astra chat interactions.
 *
 * Manages messages, tool calls, plan state, usage tracking, and agent events.
 * Connects via SSE to the Astra server for real-time streaming.
 */
type ChatState = {
  sessionId: string | null;
  runId: string | null;
  messages: ChatMessage[];
  toolCalls: ToolCall[];
  isStreaming: boolean;
  error: string | null;
  plan: PlanState | null;
  usage: TokenUsage;
  agentEvents: StreamEvent[];
  tasks: SessionTask[];
  agents: AgentActivity[];
  runStatus: string | null;
  waitingFor: string | null;
  workspace: WorkspaceBinding | undefined;
  executor: ExecutorBinding | undefined;
  transport: string | null;
  fallbackPolicy: string | null;
  connectionState: ConnectionState | "idle";
};

type ChatAction =
  | { type: "SET_SESSION_ID"; sessionId: string | null }
  | { type: "SET_RUN_ID"; runId: string | null }
  | {
      type: "SET_RUN_STATUS";
      status: string | null;
      waitingFor?: string | null;
    }
  | { type: "SET_ERROR"; error: string | null }
  | { type: "SET_STREAMING"; isStreaming: boolean }
  | { type: "SET_CONNECTION_STATE"; state: ConnectionState | "idle" }
  | {
      type: "SET_MESSAGES";
      messages: ChatMessage[] | ((prev: ChatMessage[]) => ChatMessage[]);
    }
  | { type: "SET_TOOL_CALLS"; toolCalls: ToolCall[] }
  | {
      type: "SET_PLAN";
      plan: PlanState | null | ((prev: PlanState | null) => PlanState | null);
    }
  | {
      type: "SET_USAGE";
      usage: TokenUsage | ((prev: TokenUsage) => TokenUsage);
    }
  | { type: "ADD_AGENT_EVENT"; event: StreamEvent }
  | { type: "SET_TASKS"; tasks: SessionTask[] }
  | { type: "UPSERT_AGENT"; agent: AgentActivity }
  | {
      type: "SET_WORKSPACE_BINDING";
      workspace?: WorkspaceBinding;
      executor?: ExecutorBinding;
      transport?: string;
      fallbackPolicy?: string;
    }
  | { type: "RESET" };

const initialChatState: ChatState = {
  sessionId: null,
  runId: null,
  messages: [],
  toolCalls: [],
  isStreaming: false,
  error: null,
  plan: null,
  usage: emptyUsage,
  agentEvents: [],
  tasks: [],
  agents: [],
  runStatus: null,
  waitingFor: null,
  workspace: undefined,
  executor: undefined,
  transport: null,
  fallbackPolicy: null,
  connectionState: "idle",
};

function chatReducer(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    case "SET_SESSION_ID":
      return { ...state, sessionId: action.sessionId };
    case "SET_RUN_ID":
      return { ...state, runId: action.runId };
    case "SET_RUN_STATUS":
      return {
        ...state,
        runStatus: action.status,
        waitingFor: action.waitingFor ?? null,
      };
    case "SET_ERROR":
      return { ...state, error: action.error };
    case "SET_STREAMING":
      return { ...state, isStreaming: action.isStreaming };
    case "SET_CONNECTION_STATE":
      return { ...state, connectionState: action.state };
    case "SET_MESSAGES":
      return {
        ...state,
        messages:
          typeof action.messages === "function"
            ? action.messages(state.messages)
            : action.messages,
      };
    case "SET_TOOL_CALLS":
      return { ...state, toolCalls: action.toolCalls };
    case "SET_PLAN":
      return {
        ...state,
        plan:
          typeof action.plan === "function"
            ? action.plan(state.plan)
            : action.plan,
      };
    case "SET_USAGE":
      return {
        ...state,
        usage:
          typeof action.usage === "function"
            ? action.usage(state.usage)
            : action.usage,
      };
    case "ADD_AGENT_EVENT":
      return { ...state, agentEvents: [...state.agentEvents, action.event] };
    case "SET_TASKS":
      return { ...state, tasks: action.tasks };
    case "UPSERT_AGENT": {
      const index = state.agents.findIndex(
        (agent) => agent.agentId === action.agent.agentId,
      );
      if (index < 0) {
        return { ...state, agents: [...state.agents, action.agent] };
      }
      const agents = [...state.agents];
      agents[index] = { ...agents[index], ...action.agent };
      return { ...state, agents };
    }
    case "SET_WORKSPACE_BINDING":
      return {
        ...state,
        workspace: action.workspace ?? state.workspace,
        executor: action.executor ?? state.executor,
        transport: action.transport ?? state.transport,
        fallbackPolicy: action.fallbackPolicy ?? state.fallbackPolicy,
      };
    case "RESET":
      return initialChatState;
    default:
      return state;
  }
}

export function useAstraChat(config: UseAstraChatConfig): UseAstraChatReturn {
  const [state, dispatch] = useReducer(chatReducer, {
    ...initialChatState,
    sessionId: config.sessionId ?? null,
  });

  const {
    sessionId,
    runId,
    messages,
    toolCalls,
    isStreaming,
    error,
    plan,
    usage,
    agentEvents,
    tasks,
    agents,
    runStatus,
    waitingFor,
    workspace,
    executor,
    transport,
    fallbackPolicy,
    connectionState,
  } = state;

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
      dispatch({ type: "SET_SESSION_ID", sessionId: config.sessionId ?? null });
    }
  }, [config.sessionId]);

  const processEvent = useCallback((event: StreamEvent, generation?: number) => {
    // Guard against stale events from an aborted stream leaking into the
    // new stream's state (accumulatedTextRef, toolCallMapRef, etc.).
    const expectedGeneration = generation ?? streamGenerationRef.current;
    const isStale = () => streamGenerationRef.current !== expectedGeneration;

    const upsertToolCall = (
      callId: string,
      build: (existing: ToolCall | undefined) => ToolCall,
    ) => {
      const existing = toolCallMapRef.current.get(callId);
      toolCallMapRef.current.set(callId, compactToolCall(build(existing)));
      dispatch({
        type: "SET_TOOL_CALLS",
        toolCalls: Array.from(toolCallMapRef.current.values()),
      });
    };

    const applyRunExecutionBoundary = () => {
      const fields = runBoundaryFieldsFromEvent(event);
      dispatch({
        type: "SET_WORKSPACE_BINDING",
        workspace: fields.workspace,
        executor: fields.executor,
        transport: fields.transport ?? undefined,
        fallbackPolicy: fields.fallbackPolicy ?? undefined,
      });
    };

    // Drop events from an earlier stream that completed after abort.
    if (isStale()) {
      return;
    }

    if (event.type === "run_blocked") {
      applyRunExecutionBoundary();
      const reason = extractBlockedReason(event) ?? "blocked";
      const blockedRunId = runIdFromEvent(event);
      dispatch({
        type: "SET_RUN_STATUS",
        status: "blocked",
        waitingFor: reason,
      });
      if (blockedRunId) dispatch({ type: "SET_RUN_ID", runId: blockedRunId });
      return;
    }

    switch (event.type) {
      case "session_info":
        dispatch({ type: "SET_SESSION_ID", sessionId: event.session_id });
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        break;

      case "run_started":
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({
          type: "SET_RUN_STATUS",
          status: "running",
          waitingFor: null,
        });
        break;

      case "workspace_bound":
      case "executor_bound":
      case "executor_status_changed":
        applyRunExecutionBoundary();
        break;

      case "run_paused":
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({ type: "SET_RUN_STATUS", status: "paused" });
        break;

      case "run_resumed":
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({
          type: "SET_RUN_STATUS",
          status: "running",
          waitingFor: null,
        });
        break;

      case "run_waiting": {
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        const projection = projectRunWaitingState(event);
        dispatch({
          type: "SET_RUN_STATUS",
          status: projection.status,
          waitingFor: projection.waitingFor,
        });
        break;
      }

      case "run_error":
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({
          type: "SET_RUN_STATUS",
          status: "failed",
          waitingFor: null,
        });
        dispatch({
          type: "SET_ERROR",
          error: event.message ?? event.error ?? "Astra run failed.",
        });
        dispatch({ type: "SET_STREAMING", isStreaming: false });
        dispatch({ type: "SET_CONNECTION_STATE", state: "idle" });
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return [...prev.slice(0, -1), { ...last, streaming: false }];
            }
            return prev;
          },
        });
        break;

      case "run_interrupted":
        applyRunExecutionBoundary();
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({
          type: "SET_RUN_STATUS",
          status: "paused",
          waitingFor: event.waiting_for ?? "user_resume",
        });
        dispatch({ type: "SET_STREAMING", isStreaming: false });
        dispatch({ type: "SET_CONNECTION_STATE", state: "idle" });
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return [...prev.slice(0, -1), { ...last, streaming: false }];
            }
            return prev;
          },
        });
        break;

      case "text_delta":
        accumulatedTextRef.current += event.content;
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return [
                ...prev.slice(0, -1),
                { ...last, content: accumulatedTextRef.current },
              ];
            }
            return prev;
          },
        });
        break;

      case "reasoning_delta":
        accumulatedThinkingRef.current += event.content;
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.thinking) {
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
          },
        });
        break;

      case "reasoning_done":
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.thinking) {
              return [
                ...prev.slice(0, -1),
                { ...last, thinking: { ...last.thinking, done: true } },
              ];
            }
            return prev;
          },
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
        const args = valueToToolString(event.arguments);
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
          status: toolTerminalStatus(event),
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
        dispatch({
          type: "SET_USAGE",
          usage: (prev) => ({
            promptTokens: prev.promptTokens + event.prompt_tokens,
            completionTokens: prev.completionTokens + event.completion_tokens,
            totalTokens:
              prev.totalTokens + event.prompt_tokens + event.completion_tokens,
            cacheCreationTokens:
              prev.cacheCreationTokens + (event.cache_creation_tokens ?? 0),
            cacheReadTokens:
              prev.cacheReadTokens + (event.cache_read_tokens ?? 0),
          }),
        });
        break;

      case "plan_created":
      case "plan_revised":
        dispatch({
          type: "SET_PLAN",
          plan: {
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
          },
        });
        break;

      case "plan_step_start":
        dispatch({
          type: "SET_PLAN",
          plan: (prev) =>
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
        });
        break;

      case "plan_step_done":
        dispatch({
          type: "SET_PLAN",
          plan: (prev) =>
            prev
              ? {
                  ...prev,
                  subtasks: prev.subtasks.map((s) =>
                    s.id === (event.subtask_id ?? event.step)
                      ? {
                          ...s,
                          status: planStepResultStatus(event.result),
                        }
                      : s,
                  ),
                }
              : null,
        });
        break;

      case "error":
        dispatch({ type: "SET_ERROR", error: event.message });
        break;

      case "turn_complete":
        dispatch({ type: "SET_STREAMING", isStreaming: false });
        dispatch({ type: "SET_CONNECTION_STATE", state: "idle" });
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return [...prev.slice(0, -1), { ...last, streaming: false }];
            }
            return prev;
          },
        });
        break;

      case "run_finished":
      case "run_cancelled":
        applyRunExecutionBoundary();
        dispatch({ type: "SET_STREAMING", isStreaming: false });
        dispatch({ type: "SET_CONNECTION_STATE", state: "idle" });
        if (event.run_id) dispatch({ type: "SET_RUN_ID", runId: event.run_id });
        dispatch({
          type: "SET_RUN_STATUS",
          status:
            event.type === "run_cancelled"
              ? "cancelled"
              : (event.status ?? "completed"),
          waitingFor: null,
        });
        dispatch({
          type: "SET_MESSAGES",
          messages: (prev) => {
            const last = prev[prev.length - 1];
            if (last?.role === "assistant" && last.streaming) {
              return [...prev.slice(0, -1), { ...last, streaming: false }];
            }
            return prev;
          },
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
        dispatch({ type: "ADD_AGENT_EVENT", event });
        dispatch({
          type: "UPSERT_AGENT",
          agent: agentActivityFromEvent(event),
        });
        break;

      case "task_board_snapshot":
        dispatch({ type: "SET_TASKS", tasks: event.tasks });
        break;
    }
  }, []);

  const sendMessage = useCallback(
    (content: string) => {
      if (!config.model) {
        dispatch({ type: "SET_ERROR", error: "selectedModel.model is required" });
        dispatch({ type: "SET_RUN_STATUS", status: null, waitingFor: null });
        dispatch({ type: "SET_STREAMING", isStreaming: false });
        return;
      }
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

      dispatch({
        type: "SET_MESSAGES",
        messages: (prev) => [...prev, userMsg, assistantMsg],
      });
      dispatch({ type: "SET_TOOL_CALLS", toolCalls: [] });
      dispatch({ type: "SET_ERROR", error: null });
      dispatch({ type: "SET_RUN_STATUS", status: "running", waitingFor: null });
      dispatch({ type: "SET_STREAMING", isStreaming: true });

      // Abort previous stream
      controllerRef.current?.abort();
      controllerRef.current = new AbortController();

      const sseClient = config.client.streamChat(
        {
          message: content,
          sessionId: sessionId ?? undefined,
          agentId: config.agentId,
          selectedModel: { model: config.model },
          agentBinding: config.agentBinding,
          runtimeProfile: config.runtimeProfile,
          executionBudget: config.executionBudget,
          capabilities: config.capabilities,
          explain: config.explain,
          context: config.context,
          allowSkills: config.allowSkills,
          allowTools: config.allowTools,
          enabledTools: config.enabledTools,
          skillSearch: config.skillSearch,
          workspaceBinding: config.workspaceBinding,
          executorBinding: config.executorBinding,
        },
        {
          onEvent: (event) => processEvent(event, generation),
          onStateChange: (state) =>
            dispatch({ type: "SET_CONNECTION_STATE", state }),
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
      config.agentBinding,
      config.runtimeProfile,
      config.executionBudget,
      config.capabilities,
      config.explain,
      config.context,
      config.allowSkills,
      config.allowTools,
      config.enabledTools,
      config.skillSearch,
      config.workspaceBinding,
      config.executorBinding,
      sessionId,
      processEvent,
    ],
  );

  const stop = useCallback(() => {
    streamGenerationRef.current += 1;
    controllerRef.current?.abort();
    const stoppedAt = Date.now();
    let updatedToolCalls = false;
    for (const [callId, toolCall] of toolCallMapRef.current) {
      if (toolCall.status === "running") {
        toolCallMapRef.current.set(
          callId,
          compactToolCall({
            ...toolCall,
            status: "cancelled",
            finishedAt: stoppedAt,
          }),
        );
        updatedToolCalls = true;
      }
    }
    if (updatedToolCalls) {
      dispatch({
        type: "SET_TOOL_CALLS",
        toolCalls: Array.from(toolCallMapRef.current.values()),
      });
    }
    dispatch({ type: "SET_STREAMING", isStreaming: false });
    dispatch({ type: "SET_RUN_STATUS", status: "cancelled", waitingFor: null });
    dispatch({ type: "SET_CONNECTION_STATE", state: "idle" });
    // Finalize assistant message
    dispatch({
      type: "SET_MESSAGES",
      messages: (prev: ChatMessage[]) => {
        const last = prev[prev.length - 1];
        if (last?.role === "assistant" && last.streaming) {
          return [...prev.slice(0, -1), { ...last, streaming: false }];
        }
        return prev;
      },
    });
  }, []);

  const reset = useCallback(() => {
    streamGenerationRef.current += 1;
    controllerRef.current?.abort();
    dispatch({ type: "RESET" });
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
    tasks,
    agents,
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
