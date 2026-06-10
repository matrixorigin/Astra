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
  status: "running" | "done" | "error";
  startedAt: number;
  finishedAt?: number;
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
  events?: AgentSurfaceEvent[];
  updatedAt: number;
};

export type WorkSurfaceState = {
  sessionId: string | null;
  runId: string | null;
  tasks: SessionTask[];
  tools: ToolSurfaceItem[];
  agents: AgentSurfaceItem[];
  loading: boolean;
  hydrated: boolean;
  error: string | null;
  warnings: string[];
  updatedAt: string | null;
};

export type WorkSurfaceResponse = {
  sessionId: string | null;
  runId?: string | null;
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
    tasks: [],
    tools: [],
    agents: [],
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
    tools: [],
    agents: [],
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
    tasks: response.tasks,
    tools: [],
    agents: [],
    loading: false,
    hydrated: true,
    error: null,
    warnings: response.warnings ?? [],
    updatedAt: response.generatedAt ?? new Date().toISOString(),
  };
  for (const event of response.events ?? []) {
    next = applyWorkSurfaceEvent(next, event);
  }
  return next;
}

export function applyWorkSurfaceEvent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const type = typeof event.type === "string" ? event.type : "";
  switch (type) {
    case "task_board_snapshot":
      return applyTaskBoardSnapshot(state, event);
    case "tool_call":
      return upsertToolFromToolCall(state, event);
    case "tool_call_start":
      return upsertToolFromStart(state, event);
    case "tool_call_end":
      return finishToolCall(state, event);
    case "agent_delegated":
    case "agent_spawned":
    case "agent_progress":
    case "agent_completed":
    case "agent_failed":
    case "agent_cancelled":
    case "agent_interrupted":
      return upsertAgent(state, event);
    default:
      return state;
  }
}

function applyTaskBoardSnapshot(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const tasks = Array.isArray(event.tasks)
    ? (event.tasks.filter(isTaskLike) as SessionTask[])
    : state.tasks;
  return {
    ...state,
    sessionId:
      typeof event.session_id === "string" ? event.session_id : state.sessionId,
    tasks,
    hydrated: true,
    loading: false,
    error: null,
    updatedAt: new Date().toISOString(),
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
    startedAt: timestampFromEvent(event),
  });
}

function finishToolCall(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const callId = stringField(event, "call_id");
  if (!callId) return state;
  const result = stringifyMaybe(event.result);
  const status: ToolSurfaceItem["status"] =
    event.success === false ? "error" : "done";
  const tools = upsertList(state.tools, callId, "callId", (existing) => ({
    callId,
    tool: existing?.tool ?? "tool",
    arguments: existing?.arguments,
    result,
    status,
    startedAt: existing?.startedAt ?? timestampFromEvent(event),
    finishedAt: timestampFromEvent(event),
  })).slice(-40);
  return { ...state, tools, updatedAt: new Date().toISOString() };
}

function upsertAgent(
  state: WorkSurfaceState,
  event: Record<string, unknown>,
): WorkSurfaceState {
  const agentId = stringField(event, "agent_id");
  if (!agentId) return state;
  const type = String(event.type ?? "");
  const terminalStatus =
    type === "agent_completed"
      ? "completed"
      : type === "agent_failed"
        ? "failed"
        : type === "agent_cancelled"
          ? "cancelled"
          : type === "agent_interrupted"
            ? "interrupted"
            : undefined;
  const timestamp = timestampFromEvent(event);
  const agents = upsertList(state.agents, agentId, "agentId", (existing) => {
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
        stringField(event, "status") ??
        existing?.status ??
        "running",
      toolName: stringField(event, "tool_name") ?? existing?.toolName,
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
      durationMs: numberField(event, "duration_ms") ?? existing?.durationMs,
      updatedAt: timestamp,
    };
    return {
      ...next,
      events: appendAgentEvent(existing?.events, event, next, timestamp),
    };
  }).slice(-60);
  return { ...state, agents, updatedAt: new Date().toISOString() };
}

const AGENT_EVENT_LIMIT = 32;

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

  if (type === "agent_delegated") {
    label = "Delegated";
    detail = agent.description;
    tone = "running";
  } else if (type === "agent_spawned") {
    label = "Spawned";
    detail = agent.description;
    tone = "running";
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
    type,
    status ?? "",
    label,
    detail ?? "",
    agent.turn ?? "",
    agent.toolName ?? "",
  ].join(":");
  return { id, type, label, detail, tone, timestamp };
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

function upsertTool(
  state: WorkSurfaceState,
  item: ToolSurfaceItem,
): WorkSurfaceState {
  const tools = upsertList(state.tools, item.callId, "callId", (existing) => ({
    ...existing,
    ...item,
    startedAt: existing?.startedAt ?? item.startedAt,
  })).slice(-40);
  return { ...state, tools, updatedAt: new Date().toISOString() };
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

function stringField(obj: Record<string, unknown>, key: string) {
  const value = obj[key];
  return typeof value === "string" && value.trim() ? value : undefined;
}

function numberField(obj: Record<string, unknown>, key: string) {
  const value = obj[key];
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
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
