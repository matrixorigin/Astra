import { apiFetch, apiPost, getWebDataMode, type WebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot, mockRunList } from '@/lib/api/mock-data';
import type {
  AgentSummary,
  EventSummary,
  HealthSummary,
  OverviewData,
  RunListData,
  RunSummary,
  SessionActivityData,
  SessionActivityEntry,
  SessionSummary,
} from '@/lib/models/platform';

type ApiHealthResponse = {
  status: string;
  database: string;
  persist_ok: number;
  persist_fail: number;
};

type ApiAgent = {
  agent_id: string;
  name: string;
  agent_type: string;
  owner_user_id: string;
  agent_config: Record<string, unknown>;
  is_active: boolean;
  updated_at?: string;
};

type ApiAgentListResponse = {
  agents: ApiAgent[];
  total: number;
};

type ApiSession = {
  session_id: string;
  user_id: string;
  agent_id?: string;
  title?: string;
  status: string;
  event_count: number;
  created_at: string;
  updated_at?: string;
};

type ApiSessionListResponse = {
  sessions: ApiSession[];
  total: number;
  limit: number;
  offset: number;
};

type ApiEvent = {
  event_id: string;
  session_id: string;
  event_type: string;
  content: string;
  agent_id?: string;
  created_at: string;
};

type ApiEventListResponse = {
  events: ApiEvent[];
  total: number;
  limit: number;
  offset: number;
};

type ApiReflectResponse = {
  report?: string;
  summary?: string;
  diagnosis?: string;
};

type ApiPlatformSnapshot = {
  health: ApiHealthResponse;
  agents: ApiAgentListResponse;
  sessions: ApiSessionListResponse;
  events: ApiEventListResponse;
  timestamp: string;
};

type ApiRunStatus = {
  run_id: string;
  session_id: string;
  status: string;
  waiting_for?: string;
  events_count: number;
};

type ApiRunListResponse = {
  runs: ApiRunStatus[];
  total: number;
  limit: number;
  offset: number;
};

type ApiSessionActivityEntry = {
  log_id: string;
  action: string;
  details: unknown;
  created_at: string;
};

type ApiSessionActivityResponse = {
  session_id: string;
  activities: ApiSessionActivityEntry[];
  total: number;
};

function readStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }

  return value.filter((item): item is string => typeof item === 'string');
}

function readModelName(config: Record<string, unknown>): string {
  const directModel = config.model;
  if (typeof directModel === 'string' && directModel.length > 0) {
    return directModel;
  }

  const modelName = config.model_name;
  if (typeof modelName === 'string' && modelName.length > 0) {
    return modelName;
  }

  return 'unassigned';
}

function normalizeAgent(agent: ApiAgent): AgentSummary {
  const skills =
    readStringArray(agent.agent_config.skill_filter) ||
    readStringArray(agent.agent_config.skills);

  return {
    id: agent.agent_id,
    name: agent.name,
    type: agent.agent_type,
    owner: agent.owner_user_id,
    status: agent.is_active ? 'active' : 'inactive',
    model: readModelName(agent.agent_config),
    skills,
    updatedAt: agent.updated_at,
  };
}

function normalizeSession(session: ApiSession): SessionSummary {
  return {
    id: session.session_id,
    title: session.title ?? 'Untitled session',
    owner: session.user_id,
    status: session.status,
    agentId: session.agent_id,
    eventCount: session.event_count,
    createdAt: session.created_at,
    updatedAt: session.updated_at,
  };
}

function normalizeEvent(event: ApiEvent): EventSummary {
  return {
    id: event.event_id,
    sessionId: event.session_id,
    type: event.event_type,
    summary: event.content,
    agentId: event.agent_id,
    createdAt: event.created_at,
  };
}

function normalizeHealth(health: ApiHealthResponse): HealthSummary {
  return {
    status: health.status,
    database: health.database,
    persistOk: health.persist_ok,
    persistFail: health.persist_fail,
  };
}

function buildOverviewData(
  health: HealthSummary,
  agents: AgentSummary[],
  sessions: SessionSummary[],
  events: EventSummary[],
): OverviewData {
  return {
    health,
    stats: {
      activeAgents: agents.filter((agent) => agent.status === 'active').length,
      openSessions: sessions.filter((session) => session.status !== 'closed').length,
      recentEvents: events.length,
      persistOk: health.persistOk,
    },
    agents,
    sessions,
    events,
  };
}

export function getDemoDataMode(): WebDataMode {
  return 'demo';
}

export async function getOverviewData(): Promise<OverviewData> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot;
  }

  // Try the aggregated snapshot endpoint first (single round-trip).
  try {
    const snapshot = await apiFetch<ApiPlatformSnapshot>('/platform/snapshot');
    const health = normalizeHealth(snapshot.health);
    const agents = snapshot.agents.agents.map(normalizeAgent);
    const sessions = snapshot.sessions.sessions.map(normalizeSession);
    const events = snapshot.events.events.map(normalizeEvent);
    return buildOverviewData(health, agents, sessions, events);
  } catch {
    // Fall back to individual endpoints if snapshot is unavailable.
  }

  const [health, agents, sessions, events] = await Promise.all([
    apiFetch<ApiHealthResponse>('/health'),
    apiFetch<ApiAgentListResponse>('/agents'),
    apiFetch<ApiSessionListResponse>('/sessions?limit=8'),
    apiFetch<ApiEventListResponse>('/events?limit=8'),
  ]);

  return buildOverviewData(
    normalizeHealth(health),
    agents.agents.map(normalizeAgent),
    sessions.sessions.map(normalizeSession),
    events.events.map(normalizeEvent),
  );
}

export async function getAgents(): Promise<AgentSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.agents;
  }

  const response = await apiFetch<ApiAgentListResponse>('/agents');
  return response.agents.map(normalizeAgent);
}

export async function getSessions(limit = 50): Promise<SessionSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.sessions;
  }

  const response = await apiFetch<ApiSessionListResponse>(`/sessions?limit=${limit}`);
  return response.sessions.map(normalizeSession);
}

export async function getEvents(limit = 50): Promise<EventSummary[]> {
  if ((await getWebDataMode()) === 'demo') {
    return mockPlatformSnapshot.events;
  }

  const response = await apiFetch<ApiEventListResponse>(`/events?limit=${limit}`);
  return response.events.map(normalizeEvent);
}

export async function getSessionWorkspace(sessionId: string): Promise<{
  session: SessionSummary;
  events: EventSummary[];
  reflection?: string;
  reflectionError?: string;
}> {
  if ((await getWebDataMode()) === 'demo') {
    const session = mockPlatformSnapshot.sessions.find((item) => item.id === sessionId);

    if (!session) {
      throw new Error(`Demo session not found: ${sessionId}`);
    }

    return {
      session,
      events: mockPlatformSnapshot.events.filter((event) => event.sessionId === sessionId),
      reflection: 'Demo reflection placeholder. Wire `/chat/session/{session_id}/reflect` next.',
    };
  }

  const [session, events, reflectionResult] = await Promise.all([
    apiFetch<ApiSession>(`/sessions/${sessionId}`),
    apiFetch<ApiEventListResponse>(`/events/session/${sessionId}?limit=20`),
    apiFetch<ApiReflectResponse>(`/chat/session/${sessionId}/reflect`)
      .then((value) => ({ ok: true as const, value }))
      .catch((error: Error) => ({ ok: false as const, error: error.message })),
  ]);

  const reflectionText = reflectionResult.ok
    ? reflectionResult.value.summary ??
      reflectionResult.value.report ??
      reflectionResult.value.diagnosis ??
      undefined
    : undefined;

  return {
    session: normalizeSession(session),
    events: events.events.map(normalizeEvent),
    reflection: reflectionText,
    reflectionError: reflectionResult.ok ? undefined : reflectionResult.error,
  };
}

// ── Run list ────────────────────────────────────────────────────────────────

function normalizeRun(run: ApiRunStatus): RunSummary {
  return {
    runId: run.run_id,
    sessionId: run.session_id,
    status: run.status,
    waitingFor: run.waiting_for,
    eventsCount: run.events_count,
  };
}

export async function getRuns(limit = 50, offset = 0): Promise<RunListData> {
  if ((await getWebDataMode()) === 'demo') {
    return mockRunList;
  }

  const response = await apiFetch<ApiRunListResponse>(`/runs?limit=${limit}&offset=${offset}`);
  return {
    runs: response.runs.map(normalizeRun),
    total: response.total,
    limit: response.limit,
    offset: response.offset,
  };
}

// ── Session actions ─────────────────────────────────────────────────────────

export async function resumeSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/resume`);
  return normalizeSession(response);
}

export async function cancelSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/cancel`);
  return normalizeSession(response);
}

export async function closeSession(sessionId: string): Promise<SessionSummary> {
  const response = await apiPost<ApiSession>(`/sessions/${sessionId}/close`);
  return normalizeSession(response);
}

// ── Session activity audit ──────────────────────────────────────────────────

function normalizeActivityEntry(entry: ApiSessionActivityEntry): SessionActivityEntry {
  return {
    logId: entry.log_id,
    action: entry.action,
    details: entry.details,
    createdAt: entry.created_at,
  };
}

export async function getSessionActivity(
  sessionId: string,
  limit = 100,
): Promise<SessionActivityData> {
  if ((await getWebDataMode()) === 'demo') {
    return { sessionId, activities: [], total: 0 };
  }

  const response = await apiFetch<ApiSessionActivityResponse>(
    `/sessions/${sessionId}/activity?limit=${limit}`,
  );
  return {
    sessionId: response.session_id,
    activities: response.activities.map(normalizeActivityEntry),
    total: response.total,
  };
}

// ─── Tasks / Plans ───────────────────────────────────────────────────────────

type ApiTaskListResponse = {
  tasks: Record<string, unknown>[];
  total: number;
};

type ApiTaskProgressResponse = {
  task: Record<string, unknown>;
  progress_events: {
    subtask_id: string;
    subtask_title: string;
    action: string;
    progress_pct: number;
    total_subtasks: number;
    completed_subtasks: number;
    timestamp: string;
  }[];
};

export async function getTasks(
  statusFilter?: string,
): Promise<{ tasks: Record<string, unknown>[]; total: number }> {
  const mode = await getWebDataMode();
  if (mode !== 'live') {
    return { tasks: [], total: 0 };
  }
  const qs = statusFilter ? `?status=${statusFilter}` : '';
  return apiFetch<ApiTaskListResponse>(`/tasks${qs}`);
}

export async function getTask(
  taskId: string,
): Promise<Record<string, unknown> | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    return await apiFetch<Record<string, unknown>>(`/tasks/${taskId}`);
  } catch {
    return null;
  }
}

export async function getTaskProgress(
  taskId: string,
): Promise<ApiTaskProgressResponse | null> {
  const mode = await getWebDataMode();
  if (mode !== 'live') return null;
  try {
    return await apiFetch<ApiTaskProgressResponse>(`/tasks/${taskId}/progress`);
  } catch {
    return null;
  }
}
