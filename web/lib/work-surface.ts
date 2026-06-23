import type { WorkspaceBinding, ExecutorBinding, ToolStatus } from "@astra/sdk";
import { extractEventStatus } from "@astra/sdk";
import {
  blockedRunMessage,
  runWaitingStatusMessage,
  isExecutionBoundaryWait,
  extractBlockedReason,
  projectRunWaitingState,
} from "@/lib/run-status-messages";

const MAX_SURFACE_TOOLS = 40;
const MAX_SURFACE_AGENTS = 60;

export type SessionSubtask = {
  id: string;
  title: string;
  description?: string | null;
  status: string;
  owner?: string | null;
  depends_on?: string[];
};

export type SessionTask = {
  id: string;
  title: string;
  description?: string | null;
  active_form?: string | null;
  status: string;
  owner?: string | null;
  metadata?: Record<string, unknown> | null;
  blocks?: string[];
  blocked_by?: string[];
  subtasks?: SessionSubtask[];
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
};

export type ToolSurfaceItem = {
  callId: string;
  tool: string;
  arguments?: string;
  result?: string;
  status: ToolStatus;
  errorKind?: string;
  blocked?: boolean;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string;
  fallbackPolicy?: string;
  route?: string;
  durationMs?: number;
  startedAt: number;
  finishedAt?: number;
};

export type { WorkspaceBinding, ExecutorBinding };

export type RunBlockedState = {
  reason: string;
  message: string;
  callId?: string;
  tool?: string;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string;
  fallbackPolicy?: string;
  timestamp: number;
};

export type AgentSurfaceEvent = {
  id: string;
  type: string;
  label: string;
  detail?: string;
  tone: "neutral" | "running" | "success" | "danger";
  timestamp: number;
};

export type AgentSurfaceItem = {
  agentId: string;
  runId?: string;
  parentRunId?: string;
  agentType?: string;
  description?: string;
  status: string;
  toolName?: string;
  turn?: number;
  maxTurns?: number;
  totalPromptTokens?: number;
  totalCompletionTokens?: number;
  totalToolCalls?: number;
  resultSummary?: string;
  error?: string;
  reason?: string;
  durationMs?: number;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  transport?: string;
  fallbackPolicy?: string;
  events?: AgentSurfaceEvent[];
  updatedAt: number;
};

export type WorkSurfaceState = {
  sessionId: string | null;
  runId: string | null;
  runStatus: string | null;
  workspace?: WorkspaceBinding;
  executor?: ExecutorBinding;
  tasks: SessionTask[];
  tools: ToolSurfaceItem[];
  agents: AgentSurfaceItem[];
  blocked: RunBlockedState | null;
  loading: boolean;
  hydrated: boolean;
  error: string | null;
  warnings: string[];
  updatedAt: string | null;
};

export const ACTIVE_AGENT_SURFACE_STATUSES = new Set([
  "running",
  "started",
  "busy",
  "idle",
  "tool_executing",
  "llm_call_started",
  "llm_call_completed",
  "metrics_update",
  "turn_completed",
  "permission_denied",
]);

export type WorkSurfaceResponse = {
  sessionId: string | null;
  runId?: string | null;
  status?: string | null;
  workspace?: WorkspaceBinding | null;
  executor?: ExecutorBinding | null;
  transport?: string | null;
  fallbackPolicy?: string | null;
  tasks: SessionTask[];
  events?: Record<string, unknown>[];
  generatedAt?: string;
  warnings?: string[];
};

export function createEmptyWorkSurface(
  sessionId: string | null = null,
  runId: string | null = null,
): WorkSurfaceState {
  return {
    sessionId,
    runId,
    runStatus: null,
    workspace: undefined,
    executor: undefined,
    tasks: [],
    tools: [],
    agents: [],
    blocked: null,
    loading: false,
    hydrated: false,
    error: null,
    warnings: [],
    updatedAt: null,
  };
}

export function beginWorkSurfaceLoad(
  state: WorkSurfaceState,
  sessionId: string | null,
  runId: string | null = state.runId,
): WorkSurfaceState {
  return {
    ...state,
    sessionId,
    runId,
    loading: true,
    error: null,
    warnings: [],
  };
}

export function resetWorkSurfaceForRun(
  state: WorkSurfaceState,
  params: { sessionId?: string | null; runId: string | null },
): WorkSurfaceState {
  if (
    state.runId === params.runId &&
    (params.sessionId === undefined || state.sessionId === params.sessionId)
  ) {
    return state;
  }
  return {
    ...state,
    sessionId:
      params.sessionId === undefined ? state.sessionId : params.sessionId,
    runId: params.runId,
    runStatus: null,
    workspace: undefined,
    executor: undefined,
    tools: [],
    agents: [],
    blocked: null,
    error: null,
    warnings: [],
    updatedAt: new Date().toISOString(),
  };
}

export function failWorkSurfaceLoad(
  state: WorkSurfaceState,
  message: string,
): WorkSurfaceState {
  return {
    ...state,
    loading: false,
    hydrated: true,
    error: message,
    warnings: [],
  };
}

export function hydrateWorkSurface(
  current: WorkSurfaceState,
  response: WorkSurfaceResponse,
): WorkSurfaceState {
  let next: WorkSurfaceState = {
    ...current,
    sessionId: response.sessionId,
    runId: response.runId ?? null,
    runStatus: response.status ?? null,
    workspace: response.workspace ?? undefined,
    executor: response.executor ?? undefined,
    tasks: response.tasks,
    tools: [],
    agents: [],
    blocked: null,
    loading: false,
    hydrated: true,
    error: null,
    warnings: response.warnings ?? [],
    updatedAt: response.generatedAt ?? new Date().toISOString(),
  };
  for (const event of response.events ?? []) {
    next = applyWorkSurfaceEvent(next, event);
  }
  if (response.status && isTerminalRunStatus(response.status)) {
    next = applyRunFinished(next, {
      type: "run_finished",
      run_id: response.runId ?? undefined,
      session_id: response.sessionId,
      status: response.status,
    });
  }
  return next;
}

export function applyWorkSurfaceEvent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const type = typeof event.type === "string" ? event.type : "";
  if (type === "run_blocked") {
    return applyRunBlockedEvent(state, event);
  }
  switch (type) {
    case "task_board_snapshot":
      return applyTaskBoardSnapshot(state, event);
    case "run_started":
      return applyRunStarted(state, event);
    case "run_input_queued":
      return applyRunLifecycleStatus(state, event, "input-queued");
    case "run_paused":
      return applyRunLifecycleStatus(state, event, "paused");
    case "run_waiting":
      return applyRunWaiting(state, event);
    case "run_resumed":
      return applyRunLifecycleStatus(state, event, "running");
    case "run_error":
      return applyRunError(state, event);
    case "run_interrupted":
      return applyRunInterrupted(state, event);
    case "run_finished":
      return applyRunFinished(state, event);
    case "workspace_bound":
      return applyWorkspaceBinding(state, event);
    case "executor_bound":
    case "executor_status_changed":
      return applyExecutorBinding(state, event);
    case "tool_call":
      return upsertToolFromToolCall(state, event);
    case "tool_call_start":
    case "tool_transport_started":
      return upsertToolFromStart(state, event);
    case "tool_routing_decision":
      return applyToolRoutingDecision(state, event);
    case "tool_transport_completed":
    case "tool_transport_failed":
      return finishToolTransport(state, event);
    case "tool_call_end":
      return finishToolCall(state, event);
    case "agent_delegated":
    case "agent_spawned":
    case "agent_live_event":
    case "agent_progress":
    case "agent_completed":
    case "agent_failed":
    case "agent_waiting":
    case "agent_cancelled":
    case "agent_interrupted":
      return upsertAgent(state, event);
    default:
      return state;
  }
}

function applyRunStarted(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const workspace = workspaceBindingFromEvent(event);
  const executor = executorBindingFromEvent(event);
  if (!workspace && !executor) {
    return {
      ...state,
      runId: stringField(event, "run_id") ?? state.runId,
      sessionId: stringField(event, "session_id") ?? state.sessionId,
      runStatus: "running",
      updatedAt: new Date().toISOString(),
    };
  }
  return {
    ...state,
    runId: stringField(event, "run_id") ?? state.runId,
    sessionId: stringField(event, "session_id") ?? state.sessionId,
    runStatus: "running",
    workspace: workspace ?? state.workspace,
    executor: executor ?? state.executor,
    updatedAt: new Date().toISOString(),
  };
}

function applyRunLifecycleStatus(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
  status: string,
): WorkSurfaceState {
  return {
    ...state,
    runId: stringField(event, "run_id") ?? state.runId,
    sessionId: stringField(event, "session_id") ?? state.sessionId,
    runStatus: status,
    updatedAt: new Date().toISOString(),
  };
}

function applyRunFinished(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const status = stringField(event, "status") ?? "completed";
  const timestamp = timestampFromEvent(event);
  const tools = state.tools.map((tool) =>
    tool.status === "running"
      ? finalizeRunningToolForRunStatus(tool, status, event, timestamp)
      : tool,
  );
  const agents = state.agents.map((agent) =>
    isAgentSurfaceActive(agent.status)
      ? finalizeActiveAgentForRunStatus(agent, status, event, timestamp)
      : agent,
  );
  return {
    ...state,
    runId: stringField(event, "run_id") ?? state.runId,
    sessionId: stringField(event, "session_id") ?? state.sessionId,
    runStatus: status,
    tools,
    agents,
    blocked:
      status === "completed" || status === "cancelled" ? null : state.blocked,
    updatedAt: new Date().toISOString(),
  };
}

function applyRunError(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const next = applyRunFinished(state, { ...event, status: "failed" });
  const reason = blockedReasonFromEvent(event);
  if (!reason) {
    return next;
  }
  const workspace = workspaceBindingFromEvent(event) ?? next.workspace;
  const executor = executorBindingFromEvent(event) ?? next.executor;
  return {
    ...next,
    workspace,
    executor,
    blocked: blockedStateFromEvent(event, next, workspace, executor),
  };
}

function applyRunInterrupted(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  return applyRunFinished(state, {
    ...event,
    status: "paused",
    error_kind:
      stringField(event, "error_kind") ??
      stringField(event, "kind") ??
      "interrupted",
  });
}

function applyTaskBoardSnapshot(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const tasks = Array.isArray(event.tasks)
    ? (event.tasks.filter(isTaskLike) as SessionTask[])
    : state.tasks;
  const workspace = workspaceBindingFromEvent(event);
  const executor = executorBindingFromEvent(event);
  return {
    ...state,
    sessionId:
      typeof event.session_id === "string" ? event.session_id : state.sessionId,
    runId: stringField(event, "run_id") ?? state.runId,
    workspace: workspace ?? state.workspace,
    executor: executor ?? state.executor,
    tasks,
    hydrated: true,
    loading: false,
    error: null,
    updatedAt: new Date().toISOString(),
  };
}

function applyWorkspaceBinding(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  return {
    ...state,
    sessionId:
      typeof event.session_id === "string" ? event.session_id : state.sessionId,
    workspace: workspaceBindingFromEvent(event) ?? state.workspace,
    executor: executorBindingFromEvent(event) ?? state.executor,
    updatedAt: new Date().toISOString(),
  };
}

function applyExecutorBinding(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const executor = executorBindingFromEvent(event);
  const clearBlocked = shouldClearBlockedForExecutorStatus(
    state.blocked,
    executor,
  );
  return {
    ...state,
    sessionId:
      typeof event.session_id === "string" ? event.session_id : state.sessionId,
    workspace: workspaceBindingFromEvent(event) ?? state.workspace,
    executor: executor ?? state.executor,
    runStatus:
      clearBlocked && state.runStatus === "blocked"
        ? "running"
        : state.runStatus,
    blocked: clearBlocked ? null : state.blocked,
    updatedAt: new Date().toISOString(),
  };
}

function applyRunBlockedEvent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const workspace = workspaceBindingFromEvent(event) ?? state.workspace;
  const executor = executorBindingFromEvent(event) ?? state.executor;
  return {
    ...state,
    runId: stringField(event, "run_id") ?? state.runId,
    sessionId: stringField(event, "session_id") ?? state.sessionId,
    runStatus: "blocked",
    workspace,
    executor,
    blocked: blockedStateFromEvent(event, state, workspace, executor),
    updatedAt: new Date().toISOString(),
  };
}

function applyRunWaiting(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const projection = projectRunWaitingState(
    event as {
      waiting_for?: string;
      reason?: string;
      error_kind?: string;
    },
  );
  if (projection.blocked) {
    const workspace = workspaceBindingFromEvent(event) ?? state.workspace;
    const executor = executorBindingFromEvent(event) ?? state.executor;
    return {
      ...state,
      runId: stringField(event, "run_id") ?? state.runId,
      sessionId: stringField(event, "session_id") ?? state.sessionId,
      runStatus: projection.status,
      workspace,
      executor,
      blocked: blockedStateFromEvent(
        { ...event, blocked: true, reason: projection.waitingFor },
        state,
        workspace,
        executor,
      ),
      updatedAt: new Date().toISOString(),
    };
  }
  return {
    ...applyRunLifecycleStatus(state, event, projection.status),
    blocked: null,
  };
}

function upsertToolFromToolCall(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const raw = event.tool_call;
  if (!raw || typeof raw !== "object") return state;
  const toolCall = raw as Record<string, unknown>;
  const fn =
    toolCall.function && typeof toolCall.function === "object"
      ? (toolCall.function as Record<string, unknown>)
      : {};
  const callId =
    stringField(toolCall, "id") ??
    stringField(toolCall, "call_id") ??
    stringField(fn, "id") ??
    stringField(fn, "call_id");
  const tool =
    stringField(fn, "name") ??
    stringField(toolCall, "name") ??
    stringField(toolCall, "tool");
  if (!callId || !tool) return state;
  return upsertTool(state, {
    callId,
    tool,
    arguments: stringifyMaybe(
      fn.arguments ?? toolCall.arguments ?? toolCall.args,
    ),
    status: "running",
    ...toolBindingFields(event, state),
    startedAt: timestampFromEvent(event),
  });
}

function upsertToolFromStart(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const callId = stringField(event, "call_id");
  const tool = stringField(event, "tool");
  if (!callId || !tool) return state;
  return upsertTool(state, {
    callId,
    tool,
    arguments: stringifyMaybe(event.arguments),
    status: "running",
    ...toolBindingFields(event, state),
    startedAt: timestampFromEvent(event),
  });
}

function applyToolRoutingDecision(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const callId = stringField(event, "call_id");
  if (!callId) return state;
  const route = stringField(event, "route");
  const tool = stringField(event, "tool");
  return upsertTool(state, {
    callId,
    tool: tool ?? "tool",
    status: "running",
    route,
    ...toolBindingFields(event, state),
    startedAt: timestampFromEvent(event),
  });
}

function finishToolTransport(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const callId = stringField(event, "call_id");
  if (!callId) return state;
  const isFailure =
    event.type === "tool_transport_failed" || event.success === false;
  const result = stringifyMaybe(event.error ?? event.result);
  const durationMs = numberField(event, "duration_ms");
  const timestamp = timestampFromEvent(event);
  const status = terminalToolStatus(event, isFailure);
  const tools = capToolSurfaceItems(
    upsertList(state.tools, callId, "callId", (existing) => ({
      ...existing,
      callId,
      tool: stringField(event, "tool") ?? existing?.tool ?? "tool",
      result: result ?? existing?.result,
      status,
      errorKind: stringField(event, "error_kind") ?? existing?.errorKind,
      blocked: booleanField(event, "blocked") ?? existing?.blocked,
      durationMs: durationMs ?? existing?.durationMs,
      ...toolBindingFields(event, state),
      startedAt: existing?.startedAt ?? timestamp,
      finishedAt: timestamp,
    })),
  );
  const next = applyMaybeBlockedToolFailure(
    {
      ...state,
      tools,
      updatedAt: new Date().toISOString(),
    },
    event,
  );
  return applyAgentWaitingFromToolEvent(next, event);
}

function finishToolCall(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const callId = stringField(event, "call_id");
  if (!callId) return state;
  const result = stringifyMaybe(event.result);
  const status = terminalToolStatus(event, event.success === false);
  const durationMs = numberField(event, "duration_ms");
  const tools = capToolSurfaceItems(
    upsertList(state.tools, callId, "callId", (existing) => ({
      callId,
      tool: stringField(event, "tool") ?? existing?.tool ?? "tool",
      arguments: existing?.arguments,
      result,
      status,
      errorKind: stringField(event, "error_kind") ?? existing?.errorKind,
      blocked: booleanField(event, "blocked") ?? existing?.blocked,
      workspace:
        workspaceBindingFromEvent(event) ??
        existing?.workspace ??
        state.workspace,
      executor:
        executorBindingFromEvent(event) ?? existing?.executor ?? state.executor,
      transport:
        stringField(event, "transport") ??
        existing?.transport ??
        state.executor?.transport,
      fallbackPolicy:
        stringField(event, "fallback_policy") ??
        workspaceBindingFromEvent(event)?.fallback_policy ??
        existing?.fallbackPolicy ??
        state.workspace?.fallback_policy,
      route: stringField(event, "route") ?? existing?.route,
      durationMs: durationMs ?? existing?.durationMs,
      startedAt: existing?.startedAt ?? timestampFromEvent(event),
      finishedAt: timestampFromEvent(event),
    })),
  );
  const next = applyMaybeBlockedToolFailure(
    {
      ...state,
      tools,
      updatedAt: new Date().toISOString(),
    },
    event,
  );
  return applyAgentWaitingFromToolEvent(next, event);
}

function terminalToolStatus(
  event: Record<string, unknown>,
  isFailure: boolean,
): ToolSurfaceItem["status"] {
  // A user cancellation is terminal, but it is not a tool failure.
  if (eventIsCancelled(event)) {
    return "cancelled";
  }

  // Error conditions take priority after explicit cancellations.
  if (isFailure) return "error";

  const rawStatus = extractEventStatus(
    event as Parameters<typeof extractEventStatus>[0],
  );
  if (
    rawStatus === "error" ||
    rawStatus === "failed" ||
    rawStatus === "timed_out"
  ) {
    return "error";
  }

  // Skipped is a protective dedup status, not an error
  if (rawStatus === "skipped" || booleanField(event, "skipped") === true) {
    return "skipped";
  }

  return "done";
}

function upsertAgent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const agentId = stringField(event, "agent_id");
  if (!agentId) return state;
  const type = String(event.type ?? "");
  const liveKind = stringField(event, "event_kind");
  const terminalStatus =
    type === "agent_completed"
      ? "completed"
      : type === "agent_failed"
        ? "failed"
        : type === "agent_waiting"
          ? "waiting"
          : type === "agent_cancelled"
            ? "cancelled"
            : type === "agent_interrupted"
              ? "interrupted"
              : type === "agent_live_event" && liveKind === "agent_terminated"
                ? (stringField(event, "termination") ??
                  stringField(event, "status"))
                : undefined;
  const timestamp = timestampFromEvent(event);
  const agents = capAgentSurfaceItems(
    upsertList(state.agents, agentId, "agentId", (existing) => {
      const next: AgentSurfaceItem = {
        agentId,
        runId: stringField(event, "run_id") ?? existing?.runId,
        parentRunId:
          stringField(event, "parent_run_id") ?? existing?.parentRunId,
        agentType: stringField(event, "agent_type") ?? existing?.agentType,
        description:
          stringField(event, "description") ??
          stringField(event, "task") ??
          existing?.description,
        status:
          terminalStatus ??
          liveAgentStatus(event, liveKind, existing?.status) ??
          stringField(event, "status") ??
          existing?.status ??
          "running",
        toolName:
          stringField(event, "tool_name") ??
          (liveKind?.startsWith("tool_")
            ? stringField(event, "name")
            : undefined) ??
          existing?.toolName,
        turn: numberField(event, "turn") ?? existing?.turn,
        maxTurns: numberField(event, "max_turns") ?? existing?.maxTurns,
        totalPromptTokens:
          numberField(event, "total_prompt_tokens") ??
          nestedNumberField(event, "total_tokens", "prompt") ??
          existing?.totalPromptTokens,
        totalCompletionTokens:
          numberField(event, "total_completion_tokens") ??
          nestedNumberField(event, "total_tokens", "completion") ??
          existing?.totalCompletionTokens,
        totalToolCalls:
          numberField(event, "total_tool_calls") ?? existing?.totalToolCalls,
        resultSummary:
          stringField(event, "result_summary") ??
          stringField(event, "partial_summary") ??
          existing?.resultSummary,
        error: stringField(event, "error") ?? existing?.error,
        reason: stringField(event, "reason") ?? existing?.reason,
        durationMs:
          type === "agent_live_event" && liveKind !== "agent_terminated"
            ? existing?.durationMs
            : (numberField(event, "duration_ms") ?? existing?.durationMs),
        workspace:
          workspaceBindingFromEvent(event) ??
          existing?.workspace ??
          state.workspace,
        executor:
          executorBindingFromEvent(event) ??
          existing?.executor ??
          state.executor,
        transport:
          stringField(event, "transport") ??
          existing?.transport ??
          state.executor?.transport,
        fallbackPolicy:
          stringField(event, "fallback_policy") ??
          workspaceBindingFromEvent(event)?.fallback_policy ??
          existing?.fallbackPolicy ??
          state.workspace?.fallback_policy,
        updatedAt: timestamp,
      };
      return {
        ...next,
        events: appendAgentEvent(existing?.events, event, next, timestamp),
      };
    }),
  );
  return { ...state, agents, updatedAt: new Date().toISOString() };
}

function applyAgentWaitingFromToolEvent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  if (stringField(event, "agent_status") !== "waiting") {
    return state;
  }
  const agentId = stringField(event, "agent_id");
  if (!agentId) {
    return state;
  }
  return upsertAgent(state, {
    ...event,
    type: "agent_waiting",
    agent_id: agentId,
    status: "waiting",
    reason:
      stringField(event, "reason") ??
      stringField(event, "error_kind") ??
      "waiting",
  });
}

const AGENT_EVENT_LIMIT = 32;

function liveAgentStatus(
  event: Record<string, unknown>,
  liveKind: string | undefined,
  existingStatus: string | undefined,
) {
  if (!liveKind) return undefined;
  if (liveKind === "tool_started") return "tool_executing";
  if (liveKind === "agent_terminated") {
    return stringField(event, "termination") ?? stringField(event, "status");
  }
  if (existingStatus && !ACTIVE_AGENT_SURFACE_STATUSES.has(existingStatus)) {
    return existingStatus;
  }
  return existingStatus ?? "running";
}

function appendAgentEvent(
  current: AgentSurfaceEvent[] | undefined,
  event: Record<string, unknown>,
  agent: AgentSurfaceItem,
  timestamp: number,
) {
  const entry = describeAgentEvent(event, agent, timestamp);
  const events = current ?? [];
  if (!entry) return events;

  const previous = events[events.length - 1];
  if (
    previous &&
    previous.type === entry.type &&
    previous.label === entry.label &&
    previous.tone === entry.tone &&
    (entry.type === "agent_live_event:output_delta" ||
      entry.type === "agent_live_event:thinking_delta")
  ) {
    const detail = `${previous.detail ?? ""}${entry.detail ?? ""}`.slice(-2000);
    return [
      ...events.slice(0, -1),
      { ...entry, id: previous.id, detail, timestamp },
    ];
  }
  if (
    previous &&
    previous.type === entry.type &&
    previous.label === entry.label &&
    previous.detail === entry.detail &&
    previous.tone === entry.tone
  ) {
    return [...events.slice(0, -1), { ...entry, id: previous.id }];
  }

  return [...events, entry].slice(-AGENT_EVENT_LIMIT);
}

function describeAgentEvent(
  event: Record<string, unknown>,
  agent: AgentSurfaceItem,
  timestamp: number,
): AgentSurfaceEvent | null {
  const type = String(event.type ?? "");
  const status = stringField(event, "status");
  const progress = agentProgressDetail(agent);
  let label = "Updated";
  let detail: string | undefined;
  let tone: AgentSurfaceEvent["tone"] = "neutral";
  let eventType = type;

  if (type === "agent_delegated") {
    label = "Delegated";
    detail = agent.description;
    tone = "running";
  } else if (type === "agent_spawned") {
    label = "Spawned";
    detail = agent.description;
    tone = "running";
  } else if (type === "agent_live_event") {
    const liveKind = stringField(event, "event_kind") ?? "status";
    eventType = `${type}:${liveKind}`;
    if (liveKind === "output_delta") {
      label = "Output";
      detail = stringField(event, "content");
      tone = "running";
    } else if (liveKind === "thinking_delta") {
      label = "Thinking";
      detail = stringField(event, "content");
      tone = "running";
    } else if (liveKind === "status") {
      label = "Status";
      detail = stringField(event, "content");
      tone = "running";
    } else if (liveKind === "tool_started") {
      const name = stringField(event, "name") ?? agent.toolName;
      label = name ? `Running ${name}` : "Running tool";
      detail = stringField(event, "description");
      tone = "running";
    } else if (liveKind === "tool_completed") {
      const name = stringField(event, "name") ?? agent.toolName ?? "Tool";
      const liveStatus = stringField(event, "status") ?? "ok";
      label = `${name} ${statusLabel(liveStatus)}`;
      detail =
        stringField(event, "output_summary") ??
        stringField(event, "output") ??
        stringField(event, "description");
      tone =
        liveStatus === "error" || liveStatus === "failed"
          ? "danger"
          : "success";
    } else if (liveKind === "agent_terminated") {
      const termination = stringField(event, "termination") ?? "completed";
      label = statusLabel(termination);
      detail = stringField(event, "reason");
      tone = termination === "completed" ? "success" : "danger";
    } else {
      label = statusLabel(liveKind);
      detail = stringField(event, "content");
      tone = "running";
    }
  } else if (type === "agent_progress") {
    tone = "running";
    if (status === "tool_executing") {
      label = agent.toolName ? `Running ${agent.toolName}` : "Running tool";
      detail = progress;
    } else if (status === "metrics_update") {
      label = "Metrics updated";
      detail = progress;
    } else if (status === "started") {
      label = "Started";
      detail = agent.description;
    } else if (status === "busy") {
      label = "Working";
      detail = stringField(event, "activity") ?? progress;
    } else if (status === "idle") {
      label = "Idle";
      detail = progress;
      tone = "neutral";
    } else if (status === "llm_call_started") {
      label = "Waiting for model";
      detail = turnDetail(event) ?? progress;
    } else if (status === "llm_call_completed") {
      label = "Model responded";
      detail = durationDetail(event) ?? turnDetail(event) ?? progress;
    } else if (status === "turn_completed") {
      label = "Turn completed";
      detail =
        [turnDetail(event), stringField(event, "activity")]
          .filter((item): item is string => Boolean(item))
          .join(", ") || progress;
    } else if (status === "permission_denied") {
      label = "Permission denied";
      detail =
        [
          stringField(event, "tool_name"),
          stringField(event, "reason"),
          turnDetail(event),
        ]
          .filter((item): item is string => Boolean(item))
          .join(", ") || progress;
      tone = "danger";
    } else {
      label = status ? statusLabel(status) : "Progress";
      detail = progress;
    }
  } else if (type === "agent_completed") {
    label = "Completed";
    detail = agent.resultSummary ?? progress;
    tone = "success";
  } else if (type === "agent_failed") {
    label = "Failed";
    detail = agent.error ?? agent.reason;
    tone = "danger";
  } else if (type === "agent_waiting") {
    label = "Waiting";
    detail = agent.reason ?? progress;
    tone = isExecutionBoundaryWait(agent.reason ?? "") ? "danger" : "neutral";
  } else if (type === "agent_cancelled") {
    label = "Cancelled";
    detail = agent.reason ?? agent.resultSummary;
    tone = "danger";
  } else if (type === "agent_interrupted") {
    label = "Interrupted";
    detail = agent.resultSummary ?? agent.reason ?? progress;
    tone = "danger";
  } else {
    return null;
  }

  const id = [
    timestamp,
    eventType,
    status ?? "",
    label,
    detail ?? "",
    agent.turn ?? "",
    agent.toolName ?? "",
  ].join(":");
  return { id, type: eventType, label, detail, tone, timestamp };
}

function turnDetail(event: Record<string, unknown>) {
  const turn = numberField(event, "turn");
  return turn === undefined ? undefined : `turn ${turn}`;
}

function durationDetail(event: Record<string, unknown>) {
  const duration = numberField(event, "duration_ms");
  const ttft = numberField(event, "ttft_ms");
  const parts = [
    duration === undefined ? undefined : `${duration}ms`,
    ttft === undefined ? undefined : `ttft ${ttft}ms`,
    turnDetail(event),
  ].filter((item): item is string => Boolean(item));
  return parts.length ? parts.join(", ") : undefined;
}

function agentProgressDetail(agent: AgentSurfaceItem) {
  const parts: string[] = [];
  if (agent.turn) {
    parts.push(
      `turn ${agent.turn}${agent.maxTurns ? `/${agent.maxTurns}` : ""}`,
    );
  }
  if (agent.totalToolCalls !== undefined) {
    parts.push(`${agent.totalToolCalls} tools`);
  }
  const tokens =
    (agent.totalPromptTokens ?? 0) + (agent.totalCompletionTokens ?? 0);
  if (tokens > 0) {
    parts.push(`${tokens} tokens`);
  }
  return parts.length ? parts.join(", ") : undefined;
}

function isAgentSurfaceActive(status: string) {
  return ACTIVE_AGENT_SURFACE_STATUSES.has(status);
}

function finalizeRunningToolForRunStatus(
  tool: ToolSurfaceItem,
  runStatus: string,
  event: Record<string, unknown>,
  timestamp: number,
): ToolSurfaceItem {
  if (runStatus === "completed") {
    return {
      ...tool,
      status: "done",
      result:
        tool.result ??
        "Run completed before this tool emitted a final transport result.",
      finishedAt: tool.finishedAt ?? timestamp,
    };
  }
  if (runStatus === "paused" || runStatus === "interrupted") {
    return {
      ...tool,
      status: "error",
      errorKind:
        tool.errorKind ??
        stringField(event, "error_kind") ??
        stringField(event, "kind") ??
        "interrupted",
      result: tool.result ?? defaultRunFinishedToolMessage(runStatus),
      finishedAt: tool.finishedAt ?? timestamp,
    };
  }
  const errorKind =
    runStatus === "cancelled"
      ? "cancelled"
      : (stringField(event, "error_kind") ?? runStatus);
  return {
    ...tool,
    status: runStatus === "cancelled" ? "cancelled" : "error",
    errorKind: tool.errorKind ?? errorKind,
    result:
      tool.result ??
      stringField(event, "error") ??
      stringField(event, "message") ??
      defaultRunFinishedToolMessage(runStatus),
    finishedAt: tool.finishedAt ?? timestamp,
  };
}

function finalizeActiveAgentForRunStatus(
  agent: AgentSurfaceItem,
  runStatus: string,
  event: Record<string, unknown>,
  timestamp: number,
): AgentSurfaceItem {
  if (runStatus === "completed") {
    return {
      ...agent,
      status: "completed",
      resultSummary:
        agent.resultSummary ??
        "Parent run completed before a terminal subagent event was observed.",
      updatedAt: timestamp,
    };
  }
  if (runStatus === "cancelled") {
    return {
      ...agent,
      status: "cancelled",
      reason: agent.reason ?? "parent_run_cancelled",
      resultSummary: agent.resultSummary ?? "Stopped with the parent run.",
      updatedAt: timestamp,
    };
  }
  if (runStatus === "paused" || runStatus === "interrupted") {
    return {
      ...agent,
      status: "interrupted",
      reason:
        agent.reason ??
        stringField(event, "error_kind") ??
        stringField(event, "kind") ??
        "parent_run_interrupted",
      resultSummary:
        agent.resultSummary ??
        stringField(event, "message") ??
        stringField(event, "user_message") ??
        "Parent run paused before a terminal subagent event was observed.",
      updatedAt: timestamp,
    };
  }
  return {
    ...agent,
    status: "failed",
    error:
      agent.error ??
      stringField(event, "error") ??
      stringField(event, "message") ??
      "Parent run failed before a terminal subagent event was observed.",
    reason: agent.reason ?? stringField(event, "error_kind") ?? runStatus,
    updatedAt: timestamp,
  };
}

function defaultRunFinishedToolMessage(runStatus: string) {
  if (runStatus === "cancelled") {
    return "Stopped before this tool emitted a final transport result.";
  }
  if (runStatus === "paused" || runStatus === "interrupted") {
    return "Run paused before this tool emitted a final transport result.";
  }
  return "Run failed before this tool emitted a final transport result.";
}

function eventIsCancelled(event: Record<string, unknown>) {
  return (
    booleanField(event, "cancelled") === true ||
    stringField(event, "error_kind") === "cancelled" ||
    stringField(event, "reason") === "cancelled"
  );
}

function isTerminalRunStatus(status: string) {
  return (
    status === "completed" || status === "cancelled" || status === "failed"
  );
}

function upsertTool(
  state: WorkSurfaceState,
  item: ToolSurfaceItem,
): WorkSurfaceState {
  const tools = capToolSurfaceItems(
    upsertList(state.tools, item.callId, "callId", (existing) =>
      mergeToolItem(existing, item),
    ),
  );
  return { ...state, tools, updatedAt: new Date().toISOString() };
}

function mergeToolItem(
  existing: ToolSurfaceItem | undefined,
  item: ToolSurfaceItem,
): ToolSurfaceItem {
  const next: ToolSurfaceItem = {
    ...existing,
    ...item,
    startedAt: existing?.startedAt ?? item.startedAt,
  };
  if (item.workspace === undefined) next.workspace = existing?.workspace;
  if (item.executor === undefined) next.executor = existing?.executor;
  if (item.transport === undefined) next.transport = existing?.transport;
  if (item.fallbackPolicy === undefined) {
    next.fallbackPolicy = existing?.fallbackPolicy;
  }
  if (item.route === undefined) next.route = existing?.route;
  if (item.durationMs === undefined) next.durationMs = existing?.durationMs;
  return next;
}

function applyMaybeBlockedToolFailure(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  if (eventIsCancelled(event)) {
    return state;
  }

  const reason = blockedReasonFromEvent(event);
  if (!reason) {
    return state;
  }
  const workspace = workspaceBindingFromEvent(event) ?? state.workspace;
  const executor = executorBindingFromEvent(event) ?? state.executor;
  return {
    ...state,
    runStatus: "blocked",
    workspace,
    executor,
    blocked: blockedStateFromEvent(event, state, workspace, executor),
  };
}

const ACTIONABLE_BLOCKING_ERROR_KINDS = new Set([
  "executor_offline",
  "transport_disconnected",
  "fallback_disabled",
  "workspace_executor_unavailable",
  "approval_timeout",
  "workspace_path_mismatch",
]);

function blockedReasonFromEvent(event: Record<string, unknown>) {
  const reason = extractBlockedReason(
    event as {
      type?: string;
      reason?: string;
      error_kind?: string;
      blocked?: boolean;
    },
  );
  if (reason) return reason;

  const errorKind = stringField(event, "error_kind");
  if (errorKind && ACTIONABLE_BLOCKING_ERROR_KINDS.has(errorKind)) {
    return errorKind;
  }
  return null;
}

function blockedStateFromEvent(
  event: Record<string, unknown>,
  state: WorkSurfaceState,
  workspace?: WorkspaceBinding,
  executor?: ExecutorBinding,
): RunBlockedState {
  const reason = blockedReasonFromEvent(event) ?? "blocked";
  const message =
    stringField(event, "message") ??
    stringField(event, "error") ??
    stringifyMaybe(event.result) ??
    blockedRunMessage(reason);
  return {
    reason,
    message,
    callId: stringField(event, "call_id"),
    tool: stringField(event, "tool"),
    workspace,
    executor,
    transport: stringField(event, "transport") ?? executor?.transport,
    fallbackPolicy:
      stringField(event, "fallback_policy") ??
      workspace?.fallback_policy ??
      state.workspace?.fallback_policy,
    timestamp: timestampFromEvent(event),
  };
}

function executorIsAvailable(executor: ExecutorBinding | undefined) {
  return Boolean(
    executor?.status &&
    !["offline", "degraded", "unknown"].includes(executor.status),
  );
}

function shouldClearBlockedForExecutorStatus(
  blocked: RunBlockedState | null,
  executor: ExecutorBinding | undefined,
) {
  return Boolean(
    blocked &&
    executorIsAvailable(executor) &&
    (blocked.reason === "executor_offline" ||
      blocked.reason === "transport_disconnected"),
  );
}

function upsertList<T, K extends keyof T>(
  items: T[],
  id: T[K],
  key: K,
  build: (existing: T | undefined) => T,
) {
  const index = items.findIndex((item) => item[key] === id);
  if (index === -1) return [...items, build(undefined)];
  const next = [...items];
  next[index] = build(next[index]);
  return next;
}

function capToolSurfaceItems(items: ToolSurfaceItem[]) {
  return capSurfaceItems(
    items,
    MAX_SURFACE_TOOLS,
    (item) => item.callId,
    isActiveToolSurfaceItem,
    toolSurfaceActivityTimestamp,
  );
}

function capAgentSurfaceItems(items: AgentSurfaceItem[]) {
  return capSurfaceItems(
    items,
    MAX_SURFACE_AGENTS,
    (item) => item.agentId,
    isActiveAgentSurfaceItem,
    (item) => item.updatedAt,
  );
}

function capSurfaceItems<T>(
  items: T[],
  maxItems: number,
  keyOf: (item: T) => string,
  isActive: (item: T) => boolean,
  activityTimestamp: (item: T) => number,
) {
  if (items.length <= maxItems) {
    return items;
  }
  const keep = new Set(
    [...items]
      .sort((left, right) => {
        const activeDelta = Number(isActive(right)) - Number(isActive(left));
        if (activeDelta !== 0) return activeDelta;
        return activityTimestamp(right) - activityTimestamp(left);
      })
      .slice(0, maxItems)
      .map(keyOf),
  );
  return items.filter((item) => keep.has(keyOf(item)));
}

function isActiveToolSurfaceItem(item: ToolSurfaceItem) {
  return item.status === "running";
}

function toolSurfaceActivityTimestamp(item: ToolSurfaceItem) {
  return item.finishedAt ?? item.startedAt;
}

function isActiveAgentSurfaceItem(item: AgentSurfaceItem) {
  return !["completed", "failed", "cancelled", "interrupted"].includes(
    item.status,
  );
}

function stringField(obj: Record<string, unknown>, key: string) {
  const value = obj[key];
  return typeof value === "string" && value.trim() ? value : undefined;
}

function numberField(obj: Record<string, unknown>, key: string) {
  const value = obj[key];
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : undefined;
}

function booleanField(obj: Record<string, unknown>, key: string) {
  const value = obj[key];
  return typeof value === "boolean" ? value : undefined;
}

function nestedNumberField(
  obj: Record<string, unknown>,
  key: string,
  nestedKey: string,
) {
  const value = obj[key];
  if (!value || typeof value !== "object") return undefined;
  return numberField(value as Record<string, unknown>, nestedKey);
}

function workspaceBindingFromEvent(
  event: Record<string, unknown>,
): WorkspaceBinding | undefined {
  const workspace = event.workspace;
  if (!workspace || typeof workspace !== "object") return undefined;
  const raw = workspace as Record<string, unknown>;
  const kind = stringField(raw, "kind");
  if (!kind) return undefined;
  return {
    kind,
    display_name: stringField(raw, "display_name"),
    cwd: typeof raw.cwd === "string" || raw.cwd === null ? raw.cwd : undefined,
    authority: stringField(raw, "authority"),
    fallback_policy: stringField(raw, "fallback_policy") as
      | "disabled"
      | undefined,
  };
}

function executorBindingFromEvent(
  event: Record<string, unknown>,
): ExecutorBinding | undefined {
  const executor = event.executor;
  if (!executor || typeof executor !== "object") return undefined;
  const raw = executor as Record<string, unknown>;
  const kind = normalizeExecutorKind(stringField(raw, "kind"));
  if (!kind) return undefined;
  return {
    kind,
    executor_id: stringField(raw, "executor_id"),
    display_name: stringField(raw, "display_name"),
    transport: normalizeExecutorTransport(stringField(raw, "transport")),
    status: stringField(raw, "status"),
  };
}

function normalizeExecutorKind(
  value: string | undefined,
): ExecutorBinding["kind"] | undefined {
  switch (value) {
    case "server_local":
    case "edge_agent":
    case "orchestrator_managed":
    case "thin_client":
    case "mcp":
    case "unknown":
      return value;
    case undefined:
      return undefined;
    default:
      return "unknown";
  }
}

function normalizeExecutorTransport(
  value: string | undefined,
): ExecutorBinding["transport"] | undefined {
  switch (value) {
    case "server_local":
    case "edge_ws":
    case "edge_ledger":
    case "gateway_relay":
    case "sandbox_resident_agent":
    case "mcp_http":
    case "unknown":
      return value;
    case undefined:
      return undefined;
    default:
      return "unknown";
  }
}

function toolBindingFields(
  event: Record<string, unknown>,
  state: WorkSurfaceState,
): Partial<ToolSurfaceItem> {
  const workspace = workspaceBindingFromEvent(event) ?? state.workspace;
  const executor = executorBindingFromEvent(event) ?? state.executor;
  const fields: Partial<ToolSurfaceItem> = {};
  if (workspace) fields.workspace = workspace;
  if (executor) fields.executor = executor;
  const transport = stringField(event, "transport") ?? executor?.transport;
  if (transport) fields.transport = transport;
  const fallbackPolicy =
    stringField(event, "fallback_policy") ?? workspace?.fallback_policy;
  if (fallbackPolicy) fields.fallbackPolicy = fallbackPolicy;
  const route = stringField(event, "route");
  if (route) fields.route = route;
  const durationMs = numberField(event, "duration_ms");
  if (durationMs !== undefined) fields.durationMs = durationMs;
  return fields;
}

function timestampFromEvent(event: Record<string, unknown>) {
  const value = numberField(event, "timestamp");
  return value && value > 1_000_000_000_000 ? value : Date.now();
}

function stringifyMaybe(value: unknown) {
  if (typeof value === "string") return value;
  if (value === undefined || value === null) return undefined;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function statusLabel(status: string) {
  return status.replace(/[_-]+/g, " ");
}

function isTaskLike(value: unknown): value is SessionTask {
  if (!value || typeof value !== "object") return false;
  const task = value as Record<string, unknown>;
  return (
    typeof task.id === "string" &&
    typeof task.title === "string" &&
    typeof task.status === "string"
  );
}
