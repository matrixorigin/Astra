import { apiFetch, getWebDataMode, type WebDataMode } from '@/lib/api/client';
import { mockPlatformSnapshot } from '@/lib/api/mock-data';
import type {
  AgentSummary,
  EventSummary,
  HealthSummary,
  OverviewData,
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
